# What run.sh and reset.sh share: the bounds on waiting for a server, and through gates/isolation.sh the
# readiness check. Sourced from the repository root; callers set `set -euo pipefail` themselves.
# shellcheck shell=bash

READY_TIMEOUT_S=${READY_TIMEOUT_S:-300}
HTTP_TIMEOUT_S=${HTTP_TIMEOUT_S:-10}
CLEANUP_TIMEOUT_S=${CLEANUP_TIMEOUT_S:-30}   # these three and the one below are gates/bench/run.sh's
KILL_GRACE_S=2   # between asking a process to stop and killing it (gates/bench/run.sh's value)

# shellcheck source=gates/isolation.sh
source gates/isolation.sh   # POLL_S, ping_ready and wait_ready, and the isolation steps run.sh takes

docker_cmd() { timeout --kill-after="$KILL_GRACE_S" "$HTTP_TIMEOUT_S" docker "$@"; }

stop_container() {   # stop_container NAME: a graceful stop, so the database is closed cleanly before it is copied
  timeout --kill-after="$KILL_GRACE_S" "$((CLEANUP_TIMEOUT_S + HTTP_TIMEOUT_S))" docker stop --time "$CLEANUP_TIMEOUT_S" "$1" >/dev/null
}
