#!/usr/bin/env bash
# The version-drift guard (B11), sourced by run.sh and run on the native route, between its export and Jellyfin's
# start on it: the Jellyfin release the round trip returns to starts on an empty directory, a fresh install, and the
# history it records must be the one the export wrote for the native source (native.py drift). That history is
# every id the exporter's classification of the release does not retire, so a release that adds a migration the
# exporter does not classify fails here, naming it, before any export of a database it cannot open ships. The fresh
# install's directories go once the guard passes; its log stays. It uses run.sh's state: route, dst, the settings,
# JELLYFIN_ID, MADE, and why[] through check and mark.
# shellcheck disable=SC2154  # run.sh sets what this reads

# drift_stage: the guard's check, recorded.
drift_stage() {
  local name cid=$dst/fresh.cid port from=${#why[@]}
  local what="a fresh Jellyfin records the history the export wrote, no id more or less (the drift guard)"
  name=$(rt_name "$RUN_ID" "$route" fresh)
  if ! mkdir -p "$dst/fresh" "$dst/fresh-cache"; then
    why+=("cannot create the fresh install's directories"); mark Export "$what"; return
  fi
  MADE+=("$cid")
  if port=$(rt_start "$name" "$cid" "$JELLYFIN_ID" -v "$dst/fresh:/config" -v "$dst/fresh-cache:/cache" \
      2>"$dst/fresh-start.err"); then
    rt_wait_up "$name" "$port" "$READY_TIMEOUT" "$RT_JELLYFIN_START_FAILED" ||
      why+=("the fresh install: $(rt_not_up "$name" "$READY_TIMEOUT")")
  else
    why+=("the fresh install did not start: $(head -c 300 "$dst/fresh-start.err")")
  fi
  [ -s "$dst/fresh-start.err" ] || rm -f -- "$dst/fresh-start.err"
  if [ -s "$cid" ]; then
    rt_stop "$name" "$STOP_TIMEOUT" >/dev/null   # its history is complete once it served: read it stopped
    rt_docker logs "$name" > "$dst/fresh.log" 2>&1
  fi
  rt_remove "$cid"
  [ "${#why[@]}" != "$from" ] ||
    check reasons python3 "$HERE/native.py" drift --fresh "$dst/fresh/data/jellyfin.db" --export "$dst/jellyfin/data/jellyfin.db"
  [ "${#why[@]}" != "$from" ] || rm -rf -- "${dst:?}/fresh" "${dst:?}/fresh-cache"   # a failed guard keeps them
  mark Export "$what"
}
