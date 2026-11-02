# What run.sh and reset.sh share: the bounds on waiting for a server and the readiness check.
# Sourced from the repository root; callers set `set -euo pipefail` themselves.
# shellcheck shell=bash

READY_TIMEOUT_S=${READY_TIMEOUT_S:-300}
HTTP_TIMEOUT_S=${HTTP_TIMEOUT_S:-10}
CLEANUP_TIMEOUT_S=${CLEANUP_TIMEOUT_S:-30}   # these three and the two below are gates/bench/run.sh's
KILL_GRACE_S=2   # between asking a process to stop and killing it (gates/bench/run.sh's value)
POLL_S=1         # between readiness and drain polls (gates/bench/run.sh's value)

docker_cmd() { timeout --kill-after="$KILL_GRACE_S" "$HTTP_TIMEOUT_S" docker "$@"; }

ping_ready() {   # the server at $URL answers /System/Ping and reports a version
  curl -sf --connect-timeout "$HTTP_TIMEOUT_S" --max-time "$HTTP_TIMEOUT_S" "$URL/System/Ping" >/dev/null 2>&1 &&
    curl -sf --connect-timeout "$HTTP_TIMEOUT_S" --max-time "$HTTP_TIMEOUT_S" "$URL/System/Info/Public" 2>/dev/null | jq -e '.Version | strings | length > 0' >/dev/null 2>&1
}

wait_ready() {
  local give_up=$((SECONDS + READY_TIMEOUT_S))
  while (( SECONDS < give_up )); do ping_ready && return 0; sleep "$POLL_S"; done
  return 1
}

stop_container() {   # stop_container NAME: a graceful stop, so the database is closed cleanly before it is copied
  timeout --kill-after="$KILL_GRACE_S" "$((CLEANUP_TIMEOUT_S + HTTP_TIMEOUT_S))" docker stop --time "$CLEANUP_TIMEOUT_S" "$1" >/dev/null
}
