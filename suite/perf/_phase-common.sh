# Shared bring-up for run-phase-{a,b,c}.sh. Sourced, not executed.
#
# Run in an ISOLATED compose project + host ports, so a concurrent (or hung)
# run.sh on the default `benchmark` project (ports 18096/18097) can't tear down
# our containers mid-scan — and we can't clobber it either. Overridable.
export COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-ferrofin-phase}"
export FERROFIN_HOST_PORT="${FERROFIN_HOST_PORT:-18196}"
export JELLYFIN_HOST_PORT="${JELLYFIN_HOST_PORT:-18197}"
#
# bringup_scan <svc> <base> <target>
#   (Re)create the container and run bootstrap.py (provision + scan), retrying up
#   to 3 times. Jellyfin intermittently OOMs while scanning 2636 items at the mem
#   cap, so a single failure must not doom the comparison. Returns 0 when the
#   server is scanned and ready, 1 after giving up (caller skips that server).
bringup_scan() {
  local svc="$1" base="$2" target="$3" attempt up
  echo ">> [$target] up + scan (up to 3 attempts)"
  for attempt in 1 2 3; do
    docker compose down -v >/dev/null 2>&1 || true
    # A fresh DB invalidates any ctx.json from a prior run — stale ids would 404.
    rm -f "results/raw/$target-ctx.json"
    if [ "${BENCH_SKIP_BUILD:-0}" = "1" ]; then docker compose up -d "$svc"; else docker compose up -d --build "$svc"; fi
    suite_assert_running_mounts_readonly
    up=0
    for _ in $(seq 1 120); do curl -sf "$base$SUITE_READY_PATH" >/dev/null 2>&1 && { up=1; break; }; sleep 1; done
    if [ "$up" = 1 ] && python3 bootstrap.py --target "$target" --base "$base"; then return 0; fi
    echo "   [$target] scan attempt $attempt failed — retrying"
    echo "      (an empty or missing media path looks identical here; the" \
         "container memory cap is ${BENCH_MEM})"
  done
  echo "   [$target] gave up after 3 scans — skipping this server"
  docker compose stop "$svc" >/dev/null 2>&1 || true
  return 1
}
