#!/usr/bin/env bash
# suite/lib.sh — the ONE copy of the Ferrofin↔Jellyfin bring-up (Plan 6, fixes M6 + M7).
# Sourced, never executed. Every caller cd's into suite/perf/ first (fixtures/, gen-fixtures.sh,
# docker-compose.yml all live there); these functions assume that cwd.
#
# M7 gotchas encoded here so they can't rot back into tribal knowledge:
#   - Auth uses the modern `MediaBrowser …` grammar ONLY. 10.11 ships EnableLegacyAuthorization
#     =false, so X-Emby-Token / X-Emby-Authorization are rejected by a fresh install. Never add them.
#   - DeviceId is MINTED PER STAGE (suite_mint_device_id), never a shared literal — reusing one
#     DeviceId for mid-run probes hijacks the session/playstate of whatever else uses it.
#   - suite_guard_no_probe refuses a probe while a measured k6 phase is running (would perturb it).

# Load an env file (default .env), auto-exporting every value, seeding from .env.example once.
# Also resolves the methodology knobs from suite/bench.conf FIRST (so .env / process
# env win over it — the documented order: code default < bench.conf < env).
suite_load_env() {  # $1=env file (relative to suite/perf/), default .env
  local envf="${1:-.env}"
  suite_load_bench_conf
  [ -f "$envf" ] || cp .env.example "$envf"
  set -a
  # shellcheck disable=SC1090
  . "./$envf"
  set +a
}

# Export every bench.conf KEY=value that the process environment doesn't already
# set. Python consumers resolve via suite/perf/config.py; this is the same
# contract for the shell scripts. A missing bench.conf is fine (code defaults in
# the consumers still apply).
suite_load_bench_conf() {
  local conf="../bench.conf" k v
  [ -f "$conf" ] || conf="../../suite/bench.conf"
  [ -f "$conf" ] || return 0
  while IFS='=' read -r k v; do
    case "$k" in ''|\#*) continue ;; esac
    k="${k%"${k##*[![:space:]]}"}"   # rtrim
    # Set-ness, not non-emptiness: a PRESENT-but-empty env var is a deliberate
    # override (e.g. BENCH_COLD_ENDPOINTS="" disables the cold leg) and must
    # not be resurrected from the file.
    [ -n "${!k+x}" ] || export "$k=$v"
  done < <(grep -E '^[A-Z_]+=' "$conf")
}

# Build the docker-side library list from REAL_MEDIA_DIR/REAL_TV_DIR and/or synthetic padding,
# then export LIBRARIES + EXPECTED_ITEMS + the passthrough vars every stage needs. Exits 1 if
# no media is configured. Identical JSON drives provisioning on both servers.
suite_build_libraries() {
  local libs="[" sep=""
  [ -n "${REAL_MEDIA_DIR:-}" ] && { libs="$libs${sep}{\"name\":\"Movies\",\"type\":\"movies\",\"path\":\"/media/movies-real\"}"; sep=","; }
  [ -n "${REAL_TV_DIR:-}" ]    && { libs="$libs${sep}{\"name\":\"Shows\",\"type\":\"tvshows\",\"path\":\"/media/tv-real\"}"; sep=","; }
  [ "${FIXTURE_MOVIES:-0}" -gt 0 ] && { libs="$libs${sep}{\"name\":\"Movies (synth)\",\"type\":\"movies\",\"path\":\"/media/synth/movies\"}"; sep=","; }
  [ "${FIXTURE_SERIES:-0}" -gt 0 ] && { libs="$libs${sep}{\"name\":\"Shows (synth)\",\"type\":\"tvshows\",\"path\":\"/media/synth/tv\"}"; sep=","; }
  [ "${FIXTURE_ARTISTS:-0}" -gt 0 ] && { libs="$libs${sep}{\"name\":\"Music (synth)\",\"type\":\"music\",\"path\":\"/media/synth/music\"}"; sep=","; }
  libs="$libs]"
  [ "$libs" = "[]" ] && { echo "No media: set REAL_MEDIA_DIR or FIXTURE_MOVIES>0 in the env file" >&2; exit 1; }
  # Live TV fixture: the M3U tuner + XMLTV guide both servers are provisioned with (paths on
  # the shared mount; the channel streams themselves come from the livetv-source sidecar).
  if [ "${FIXTURE_LIVETV:-0}" -gt 0 ]; then
    export LIVETV_M3U=/media/synth/livetv/channels.m3u LIVETV_XMLTV=/media/synth/livetv/guide.xml
  else
    export LIVETV_M3U="" LIVETV_XMLTV=""
  fi

  # Real counts are unknown up front (naming resolvers diverge on real files); the scan waiter
  # settles on a stable total instead. Synthetic count is exact and known.
  if [ -n "${REAL_MEDIA_DIR:-}" ] || [ -n "${REAL_TV_DIR:-}" ]; then EXPECTED_ITEMS=0
  else EXPECTED_ITEMS=$(( ${FIXTURE_MOVIES:-0} + ${FIXTURE_SERIES:-0} * ${FIXTURE_EPISODES_PER_SERIES:-0} )); fi

  export LIBRARIES="$libs" EXPECTED_ITEMS REAL_MEDIA_DIR REAL_TV_DIR \
         BENCH_ADMIN_USER BENCH_ADMIN_PASSWORD JELLYFIN_IMAGE
}

# Generate synthetic fixtures once, only when padding is requested and none exist yet — or
# when a requested library (music) is missing from an older fixture tree.
suite_gen_fixtures() {
  if { [ "${FIXTURE_MOVIES:-0}" -gt 0 ] || [ "${FIXTURE_SERIES:-0}" -gt 0 ]; } && \
     [ -z "$(find fixtures/media -type f 2>/dev/null | head -1)" ]; then
    echo ">> generating synthetic fixtures"; ./gen-fixtures.sh
  elif [ "${FIXTURE_ARTISTS:-0}" -gt 0 ] && [ ! -d fixtures/media/music ]; then
    echo ">> regenerating synthetic fixtures (music library requested, absent)"; ./gen-fixtures.sh
  elif [ "${FIXTURE_LIVETV:-0}" -gt 0 ] && [ ! -d fixtures/media/livetv ]; then
    echo ">> regenerating synthetic fixtures (live tv fixture requested, absent)"; ./gen-fixtures.sh
  fi
}

# Poll a base URL until its first 200, or die. Cold-start / readiness gate for a stage.
# The ONE readiness route for every poll in the suite. NOT /System/Info/Public:
# Jellyfin 10.11 binds a SetupServer stub that serves exactly that one route —
# and nothing else — while the real ApplicationHost is still starting, then
# drops the socket. Polling it reports "ready" before the server can serve
# anything (measured on an empty DB: stub 200 at t=1.3s, socket gone at 1.5s,
# real server at 2.7s; the gap scales to ~20s on a populated DB). That
# understated Jellyfin's cold-start and raced bring-up into `auth failed: 0`.
# /Users/Public is unauthenticated, absent from the stub, 503 while the real app
# starts and 200 only once it can serve; Ferrofin answers it and
# /System/Info/Public simultaneously, so this costs Ferrofin nothing. It is also
# absent from BENCH_COLD_ENDPOINTS, so polling it cannot pre-warm a cold
# sentinel — which the old probe did to info_public, its own sentinel.
SUITE_READY_PATH="/Users/Public"

suite_wait200() {  # $1=base url $2=name
  local _
  for _ in $(seq 1 120); do curl -sf "$1$SUITE_READY_PATH" >/dev/null 2>&1 && return 0; sleep 0.5; done
  echo "$2 never came up" >&2; exit 1
}

# ── auth (M7: one grammar, minted DeviceId) ────────────────────────────────────
# Mint a DeviceId unique to this stage+process. Call once per stage before any probe.
suite_mint_device_id() { export SUITE_DEVICE_ID="suite-${1:-probe}-$$"; }
# The client tuple used in every Authorization header. DeviceId comes from the mint above.
suite_client_id() { echo "Client=\"suite\", Device=\"suite\", DeviceId=\"${SUITE_DEVICE_ID:-suite}\", Version=\"1.0\""; }

# Authenticate and echo "<token> <userId>" (empty on failure).
suite_auth() {  # $1=base url
  local resp
  resp=$(curl -sf -X POST "$1/Users/AuthenticateByName" -H 'Content-Type: application/json' \
    -H "Authorization: MediaBrowser $(suite_client_id)" \
    -d "{\"Username\":\"${BENCH_ADMIN_USER:-bench}\",\"Pw\":\"${BENCH_ADMIN_PASSWORD:-benchpass123}\"}") || return 1
  echo "$(echo "$resp" | jq -r .AccessToken) $(echo "$resp" | jq -r .User.Id)"
}

# Authoritative Movie,Episode item count — the fairness figure (folders resolve per-server).
suite_count_items() {  # $1=base url  [$2=token $3=userId]
  suite_guard_no_probe || { echo "?"; return; }
  local tok uid pair
  # Reuse a pre-minted token when given ($2/$3): the perf leg's auth_login scenario
  # throttles the login endpoint, so a fresh auth here would 500 against Jellyfin.
  if [ -n "${2:-}" ]; then
    tok="$2"; uid="$3"
  else
    pair=$(suite_auth "$1") || { echo "?"; return; }
    tok=${pair%% *}; uid=${pair##* }
  fi
  curl -sf "$1/Items?userId=$uid&Recursive=true&IncludeItemTypes=Movie,Episode&Limit=0" \
    -H "Authorization: MediaBrowser Token=\"$tok\", $(suite_client_id)" \
    | jq -r '.TotalRecordCount // "?"'
}

# Refuse a probe while a measured load phase is active — probes perturb the measurement,
# and a reused session/DeviceId can corrupt it. Perf stages set SUITE_MEASURE_ACTIVE=1
# around each measured window (SUITE_K6_ACTIVE honored for muscle memory).
suite_guard_no_probe() {
  if [ "${SUITE_MEASURE_ACTIVE:-0}" = 1 ] || [ "${SUITE_K6_ACTIVE:-0}" = 1 ]; then
    echo "!! refusing probe: a measured load phase is active" >&2; return 1
  fi
  return 0
}
