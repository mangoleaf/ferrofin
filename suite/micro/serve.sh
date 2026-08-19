#!/usr/bin/env bash
# Boots the profiling binary over an already-scanned database copy, so the fast
# loop never pays a Docker build or a library scan. See README.md.
#
#   ./serve.sh start     background, waits until it answers
#   ./serve.sh profile    foreground under samply (ctrl-c -> profiler UI)
#   ./serve.sh stop
#   ./serve.sh status
set -euo pipefail
cd "$(dirname "$0")"

DATA=${FF_MICRO_DATA:-/tmp/ff-fast}
PORT=${FF_MICRO_PORT:-18299}
BIN=../../target/profiling/ferrofin-server
PIDFILE=/tmp/ff-micro.pid
LOG=/tmp/ff-micro.log

# The bench fixture's admin. Same values the suite's .env uses, because the
# database being served was scanned by the suite and already holds this user.
export FERROFIN_ADMIN_USER=${FERROFIN_ADMIN_USER:-bench}
export FERROFIN_ADMIN_PASSWORD=${FERROFIN_ADMIN_PASSWORD:-benchpass123}
# Never let a background scan or a plugin task run during a measurement — the
# whole point of this harness is that only the endpoint under test moves.
export FERROFIN_DISABLE_EXTENSIONS=${FERROFIN_DISABLE_EXTENSIONS:-1}

args=(--data-dir "$DATA" --bind 127.0.0.1 --port "$PORT")

case "${1:-start}" in
  start)
    [ -x "$BIN" ] || { echo "missing $BIN — cargo build --profile profiling -p ferrofin-server" >&2; exit 1; }
    if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
      echo "already running (pid $(cat "$PIDFILE")) on :$PORT"; exit 0
    fi
    "$BIN" "${args[@]}" > "$LOG" 2>&1 &
    echo $! > "$PIDFILE"
    for _ in $(seq 1 120); do
      if curl -sf "http://127.0.0.1:$PORT/System/Info/Public" >/dev/null 2>&1; then
        echo "up on :$PORT (pid $(cat "$PIDFILE")), data=$DATA"; exit 0
      fi
      kill -0 "$(cat "$PIDFILE")" 2>/dev/null || { echo "died on boot — tail $LOG:" >&2; tail -20 "$LOG" >&2; exit 1; }
      sleep 0.5
    done
    echo "did not answer within 60s — tail $LOG:" >&2; tail -20 "$LOG" >&2; exit 1
    ;;
  profile)
    command -v samply >/dev/null || { echo "samply not installed: cargo install samply" >&2; exit 1; }
    echo ">> samply on :$PORT — drive it with ./hit.sh, then ctrl-c for the UI"
    exec samply record -- "$BIN" "${args[@]}"
    ;;
  stop)
    [ -f "$PIDFILE" ] || { echo "not running"; exit 0; }
    kill "$(cat "$PIDFILE")" 2>/dev/null || true
    rm -f "$PIDFILE"; echo "stopped"
    ;;
  status)
    if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
      echo "running (pid $(cat "$PIDFILE")) on :$PORT"
    else echo "not running"; fi
    ;;
  *) echo "usage: $0 {start|profile|stop|status}" >&2; exit 2 ;;
esac
