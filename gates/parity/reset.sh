#!/usr/bin/env bash
# Give both parity servers a fresh copy of the database before a write: stop each container, put back
# the isolated configuration and cache run.sh snapshotted at boot (RUN_DIR/<server>/pristine), start it
# again, and wait until it answers. Both servers are reset at once. diff.py runs this before every
# write and signs in again afterwards (the fresh copy holds no session of this run).
#
#   reset.sh RUN_DIR
#
# Exit 0 when both servers answer again; 1 with one line per server that did not, on stderr.
# Settings (environment, exported by run.sh): READY_TIMEOUT_S, HTTP_TIMEOUT_S, CLEANUP_TIMEOUT_S.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=gates/parity/lib.sh
source gates/parity/lib.sh
if [ $# -ne 1 ] || [ ! -d "$1" ]; then echo "usage: reset.sh RUN_DIR" >&2; exit 2; fi
RUN=$1

reset_server() {   # reset_server NAME: runs in a background subshell; says what failed on stdout, one line
  local name=$1 d=$RUN/$1
  URL=$(cat "$d/url") || { echo "$name: no url recorded at boot"; return 1; }
  if [ ! -d "$d/pristine/config" ] || [ ! -d "$d/pristine/cache" ]; then echo "$name: no snapshot taken at boot"; return 1; fi
  stop_container "parity-$name" || { echo "$name: could not stop it"; return 1; }
  rm -rf -- "${d:?}/config" "${d:?}/cache" || { echo "$name: cannot remove the state the last write left"; return 1; }
  if ! cp -r --reflink=auto "$d/pristine/config" "$d/config" || ! cp -r --reflink=auto "$d/pristine/cache" "$d/cache"; then
    echo "$name: cannot restore the snapshot"; return 1
  fi
  docker_cmd start "parity-$name" >/dev/null || { echo "$name: docker could not start it again"; return 1; }
  wait_ready || { echo "$name: did not answer /System/Ping within ${READY_TIMEOUT_S}s of the reset"; return 1; }
}

# A signal stops the two resets (not what they are waiting on: diff.py and run.sh kill this whole session
# for that, the docker calls included).
# shellcheck disable=SC2317,SC2329  # invoked by the trap (SC2317 is the code before shellcheck 0.11)
stop_resets() { local job; for job in $(jobs -p); do kill "$job" 2>/dev/null; done; exit 143; }
trap stop_resets TERM INT HUP
# Each server's one-line verdict goes to reset.out, what the tools said on the way to reset.log.
for name in oracle ferrofin; do : > "$RUN/$name/reset.out"; : > "$RUN/$name/reset.log"; done   # this reset's lines only
reset_server oracle > "$RUN/oracle/reset.out" 2>> "$RUN/oracle/reset.log" & oracle=$!
reset_server ferrofin > "$RUN/ferrofin/reset.out" 2>> "$RUN/ferrofin/reset.log" & ferrofin=$!
failed=0
wait "$oracle" || failed=1
wait "$ferrofin" || failed=1
if (( failed )); then
  for name in oracle ferrofin; do
    [ -s "$RUN/$name/reset.out" ] || continue
    tail -n 3 "$RUN/$name/reset.log" 2>/dev/null | sed "s/^/$name: /" >&2   # what docker or cp said, before the verdict
    cat "$RUN/$name/reset.out" >&2
  done
  grep -qs . "$RUN/oracle/reset.out" "$RUN/ferrofin/reset.out" || echo "a reset failed without saying why (see */reset.log)" >&2
  exit 1
fi
