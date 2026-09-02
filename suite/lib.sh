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
  # What the caller already set wins. Sourcing overwrote it, so a fresh
  # worktree — whose `.env` is the copied TEMPLATE, with
  # `REAL_MEDIA_DIR=/path/to/your/movies` — silently beat an explicit
  # `REAL_MEDIA_DIR=... ./perf-gate.sh`, mounted a path that does not exist, and
  # scanned an empty library. Same "set-ness, not non-emptiness" rule as
  # `suite_load_bench_conf`; the file is still SOURCED (it has comments and
  # quoting), the caller's values are just put back afterwards.
  local preset=() k
  while IFS='=' read -r k _; do
    case "$k" in ''|\#*) continue ;; esac
    k="${k%"${k##*[![:space:]]}"}"
    [ -n "${!k+x}" ] && preset+=("$k=${!k}")
  done < <(grep -E '^[A-Z_]+=' "$envf")
  set -a
  # shellcheck disable=SC1090
  . "./$envf"
  set +a
  local kv
  for kv in ${preset+"${preset[@]}"}; do export "${kv?}"; done
}

# The suite mounts NO real media (plan §E2) — a leftover REAL_MEDIA_DIR /
# REAL_TV_DIR in a stale .env would register libraries at nonexistent container
# paths and fail minutes later with a misleading scan message. Refused by
# suite_require_media, which every entry script runs BEFORE the results/raw
# wipe: aborting after the wipe would destroy the previous run's raw results
# (merge on them is the documented recovery path).
suite_refuse_real_media_env() {
  if [ -n "${REAL_MEDIA_DIR:-}" ] || [ -n "${REAL_TV_DIR:-}" ]; then
    echo "REAL_MEDIA_DIR/REAL_TV_DIR are no longer supported: the suite mounts no real" >&2
    echo "media (plan §E2). Remove them from suite/perf/.env and use BENCH_TESTDATA=1." >&2
    exit 1
  fi
}
# Enforced pre-wipe for the same reason as the refusal above: the snapshot's
# media files are deliberately absent, so a transcode leg has nothing to serve,
# and aborting later (in suite_build_libraries) would first destroy the
# previous run's raw results.
suite_refuse_transcode_in_testdata() {
  if [ "${BENCH_TESTDATA:-0}" = 1 ] && [ "${RUN_TRANSCODE:-0}" = 1 ]; then
    echo "RUN_TRANSCODE=1 is impossible with BENCH_TESTDATA=1 — the snapshot's media files are absent" >&2
    exit 1
  fi
}
suite_require_media() { suite_refuse_real_media_env; suite_refuse_transcode_in_testdata; }

# The owner's irreplaceable data, protected UNCONDITIONALLY — not only when an
# env var happens to point at it. Any bind mount that could reach under these
# (the root itself, a subpath, or a parent directory of one) must be read-only.
SUITE_PROTECTED_MEDIA_ROOTS="/mnt/mangonas /mnt/nvme0/k3s"

# HARD INVARIANT: the suite never writes to real media. Every host path mounted
# into a server container under /media MUST be read-only, and this refuses to
# start if one is not.
#
# It is not paranoia about the mount line alone. A server booted on a real
# config directory carries that instance's library options, which may include
# "save metadata/artwork into media folders" — a scan would then write .nfo and
# image files next to the owner's media. `:ro` is what makes that attempt fail
# harmlessly instead of mutating an irreplaceable library, so it may never be
# optional and may never be quietly dropped by an edit.
# Pre-flight, over the RESOLVED compose config, never the YAML text: the media
# mounts here are `${REAL_MEDIA_DIR:-…}:/media/…` lines, and a text parse cannot
# see through the interpolation (it read `${REAL_MEDIA_DIR` as the host source).
# `docker compose config` resolves variables and normalises every volume to long
# form, so this checks the mounts `up` would actually create. Keyed on both the
# container path (/media…) and the real host roots, and it FAILS CLOSED: if the
# config cannot be resolved, the run does not start.
suite_assert_media_readonly() {
  docker compose config --format json 2>/dev/null \
    | MEDIA_ROOTS="${REAL_MEDIA_DIR:-} ${REAL_TV_DIR:-} ${TESTDATA_MEDIA_ROOTS:-} $SUITE_PROTECTED_MEDIA_ROOTS" python3 -c '
import json, os, sys
cfg = json.load(sys.stdin)
# NOTE: roots are whitespace-split — a root containing spaces or glob chars
# would mis-match. Fine for the real paths in use; do not add one with spaces.
roots = [r.rstrip("/") for r in os.environ.get("MEDIA_ROOTS", "").split() if r]
def touches(src, r):
    # Either containment direction is media-reaching: a mount OF the root or a
    # subpath, and a mount of a PARENT (e.g. /mnt/nvme0/k3s:/data, or / itself)
    # through which the root is writable.
    src = src.rstrip("/")   # "/" normalises to "", and "".startswith→parent-of-all
    return src == r or src.startswith(r + "/") or r.startswith(src + "/")
bad = []
for name, svc in (cfg.get("services") or {}).items():
    for v in svc.get("volumes") or []:
        if not isinstance(v, dict):   # config emits long form; anything else is unresolvable
            bad.append(f"{name}: unparsed volume entry {v!r}"); continue
        src, tgt = v.get("source") or "", v.get("target") or ""
        is_media = tgt == "/media" or tgt.startswith("/media/") \
            or (v.get("type") == "bind" and any(touches(src, r) for r in roots))
        if is_media and not v.get("read_only"):
            bad.append(f"{name}: {src} -> {tgt} is NOT read-only")
# A named volume can smuggle a bind via driver_opts (o: bind, device: <path>);
# its service-level source is then just the volume name and the runtime mount
# source is /var/lib/docker/volumes/…, so neither check above sees the device.
# Refuse the construction outright if the device reaches protected media.
for vname, vdef in (cfg.get("volumes") or {}).items():
    dev = ((vdef or {}).get("driver_opts") or {}).get("device") or ""
    if dev and any(touches(dev, r) for r in roots):
        bad.append(f"volume {vname}: driver_opts.device {dev} reaches protected media")
for b in bad:
    print(f"media mount check: {b}", file=sys.stderr)
sys.exit(1 if bad else 0)
' || {
    echo "" >&2
    echo "REFUSING TO START. The suite must never be able to write to real media;" >&2
    echo "every media mount has to be read-only (or docker compose config failed)." >&2
    exit 1
  }
}

# Post-start: assert against REALITY, not the compose text. Reads the running
# containers' actual mounts and refuses if any media mount came up writable —
# the compose file being right is not the same as the mount being right.
suite_assert_running_mounts_readonly() {
  local bad=0 cid cids name dest rw src mounts n=0
  # Fail CLOSED, like the pre-flight: this runs right after an `up`, so an
  # errored or empty listing means the guard cannot see what it must verify —
  # never that everything is fine.
  cids=$(docker compose ps -q) || cids=""
  for cid in $cids; do
    n=$((n + 1))
    name=$(docker inspect -f '{{.Name}}' "$cid" 2>/dev/null) || name="$cid"
    mounts=$(docker inspect -f '{{range .Mounts}}{{.Destination}}|{{.RW}}|{{.Source}}
{{end}}' "$cid") || { echo "docker inspect $name failed; refusing" >&2; bad=1; continue; }
    while IFS='|' read -r dest rw src; do
      [ -n "$dest" ] || continue
      local is_media=0
      case "$dest" in /media*) is_media=1;; esac
      case "$src" in /) is_media=1;; esac   # a bind of / reaches everything
      local r
      for r in ${REAL_MEDIA_DIR:-} ${REAL_TV_DIR:-} ${TESTDATA_MEDIA_ROOTS:-} $SUITE_PROTECTED_MEDIA_ROOTS; do
        [ -n "$r" ] || continue
        r=${r%/}   # a trailing slash on an env root must not break the subpath pattern
        case "$src" in "$r"|"$r"/*) is_media=1;; esac   # the root, or under it
        case "$r" in "$src"|"$src"/*) is_media=1;; esac # a parent of the root
      done
      [ "$is_media" = 1 ] || continue
      if [ "$rw" = "true" ]; then
        echo "!! $name has $src -> $dest mounted WRITABLE" >&2
        bad=1
      fi
    done <<<"$mounts"
  done
  if [ "$n" -lt 1 ]; then
    echo "REFUSING TO PROCEED: no running containers found to verify after up" >&2
    bad=1
  fi
  if [ "$bad" != 0 ]; then
    echo "REFUSING TO PROCEED: a running container can write to real media (or could not be verified)." >&2
    docker compose down -v >/dev/null 2>&1 || true
    exit 1
  fi
}

# The synth half of the same guard, as a POSTcondition of suite_gen_fixtures —
# before it, an empty fixtures/media is the normal fresh-worktree state, not an
# error. Uses `find -type f`, not `ls -A`: run.sh pre-creates
# fixtures/media/{movies,tv}, so a directory test can never fire.
suite_require_fixtures() {
  local synth=0
  [ "${FIXTURE_MOVIES:-0}" -gt 0 ] && synth=1
  [ "${FIXTURE_SERIES:-0}" -gt 0 ] && synth=1
  [ "${FIXTURE_ARTISTS:-0}" -gt 0 ] && synth=1
  [ "$synth" = 1 ] || return 0
  [ -n "$(find fixtures/media -type f 2>/dev/null | head -1)" ] && return 0
  echo "synthetic fixtures are requested but generation produced no files under" >&2
  echo "  suite/perf/fixtures/media (check ffmpeg is installed and gen-fixtures.sh" >&2
  echo "  ran — or use BENCH_TESTDATA=1, which needs no synthetic video fixtures)" >&2
  exit 1
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

# Build the docker-side library list (testdata mode: none — the seeded
# snapshot IS the library; synthetic mode: from the FIXTURE_* counts), then
# export LIBRARIES + EXPECTED_ITEMS + the passthrough vars every stage needs.
# Identical JSON drives provisioning on both servers.
suite_build_libraries() {
  suite_refuse_real_media_env
  # Testdata mode (BENCH_TESTDATA=1, plan §E): the seeded snapshot IS the
  # library — provisioning must add nothing and scan nothing. Adding a library
  # or kicking a refresh would validate libraries whose media paths are
  # deliberately absent, and what a scan does to path-less items is
  # server-specific: the one way this mode could diverge the two datasets.
  if [ "${BENCH_TESTDATA:-0}" = 1 ]; then
    suite_refuse_transcode_in_testdata   # belt: entry scripts check pre-wipe via suite_require_media
    # Live TV stays: registering an M3U tuner + XMLTV guide is additive (no
    # library validation touches it) and the fixture lives on the synth mount.
    if [ "${FIXTURE_LIVETV:-0}" -gt 0 ]; then
      export LIVETV_M3U=/media/synth/livetv/channels.m3u LIVETV_XMLTV=/media/synth/livetv/guide.xml
    else
      export LIVETV_M3U="" LIVETV_XMLTV=""
    fi
    export LIBRARIES="[]" EXPECTED_ITEMS=0 \
           BENCH_ADMIN_USER BENCH_ADMIN_PASSWORD JELLYFIN_IMAGE
    return 0
  fi
  local libs="[" sep=""
  [ "${FIXTURE_MOVIES:-0}" -gt 0 ] && { libs="$libs${sep}{\"name\":\"Movies (synth)\",\"type\":\"movies\",\"path\":\"/media/synth/movies\"}"; sep=","; }
  [ "${FIXTURE_SERIES:-0}" -gt 0 ] && { libs="$libs${sep}{\"name\":\"Shows (synth)\",\"type\":\"tvshows\",\"path\":\"/media/synth/tv\"}"; sep=","; }
  [ "${FIXTURE_ARTISTS:-0}" -gt 0 ] && { libs="$libs${sep}{\"name\":\"Music (synth)\",\"type\":\"music\",\"path\":\"/media/synth/music\"}"; sep=","; }
  libs="$libs]"
  [ "$libs" = "[]" ] && { echo "No libraries: set FIXTURE_MOVIES>0 in the env file, or use BENCH_TESTDATA=1" >&2; exit 1; }
  # Live TV fixture: the M3U tuner + XMLTV guide both servers are provisioned with (paths on
  # the shared mount; the channel streams themselves come from the livetv-source sidecar).
  if [ "${FIXTURE_LIVETV:-0}" -gt 0 ]; then
    export LIVETV_M3U=/media/synth/livetv/channels.m3u LIVETV_XMLTV=/media/synth/livetv/guide.xml
  else
    export LIVETV_M3U="" LIVETV_XMLTV=""
  fi

  # Synthetic count is exact and known.
  EXPECTED_ITEMS=$(( ${FIXTURE_MOVIES:-0} + ${FIXTURE_SERIES:-0} * ${FIXTURE_EPISODES_PER_SERIES:-0} ))

  export LIBRARIES="$libs" EXPECTED_ITEMS \
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

# ── Testdata mode (BENCH_TESTDATA=1, PLAN_SUITE_TRUSTWORTHY §E) ──────────────
# The suite's data is a pinned, read-only snapshot of a real Jellyfin instance
# at suite/test_data/ (see suite/snapshot-testdata.sh). Each server gets its
# own writable COPY seeded into its compose config volume before it starts:
# Jellyfin reads it natively, Ferrofin adopts it (adoption MUTATES the db,
# which is why nothing may ever boot the pin itself). No media is mounted at
# all — browse/query/user-data come from the db, images from metadata/.

SUITE_TESTDATA_PIN=../test_data                     # relative to suite/perf/
SUITE_TESTDATA_STAGE=../../target/bench-testdata-stage   # under target/: gitignored, same fs as the pin

# Stage the pin once: hardlink the tree (cheap, same filesystem), REAL-copy the
# database (a hardlinked db + sqlite UPDATE would write through into the pin),
# strip what must not ride along, and reset the admin user's password to the
# bench credentials so AuthenticateByName (a measured endpoint) works without
# shipping real credentials anywhere. Re-staged only when the pin changes.
suite_stage_testdata() {
  # MANIFEST.json is tracked, the data is not — a fresh worktree/clone has the
  # manifest and nothing else, so the guard must key on the database itself.
  [ -f "$SUITE_TESTDATA_PIN/MANIFEST.json" ] && [ -f "$SUITE_TESTDATA_PIN/data/jellyfin.db" ] || {
    echo "testdata pin missing or dataless: suite/test_data — run suite/snapshot-testdata.sh" >&2; exit 1; }
  local stage="$SUITE_TESTDATA_STAGE"
  if [ -f "$stage/MANIFEST.json" ] && cmp -s "$stage/MANIFEST.json" "$SUITE_TESTDATA_PIN/MANIFEST.json"; then
    suite_export_testdata_user
    # BENCH_ADMIN_PASSWORD may have changed since the stage was built; the
    # reset is idempotent and only touches the stage db's own inode. If it
    # fails (stage db missing/corrupt despite the manifest), fall through and
    # rebuild the stage instead of wedging every subsequent run.
    if suite_stage_set_password; then return 0; fi
    echo ">> stage unusable (password reset failed) — restaging" >&2
  fi
  echo ">> staging testdata snapshot ($(command grep -o '"base_items": [0-9]*' "$SUITE_TESTDATA_PIN/MANIFEST.json" || true))"
  # A stale stage can carry read-only dirs (interrupted run) that rm cannot
  # descend — restore write bits on DIRECTORIES ONLY first: its files are
  # hardlinks into the pin, and chmod on one writes through to the pin's
  # shared inode. (Unlinking a read-only file needs only a writable parent.)
  # Literal path, never a variable.
  if [ -d ../../target/bench-testdata-stage ]; then
    find ../../target/bench-testdata-stage -type d -exec chmod u+w {} +
    rm -rf -- ../../target/bench-testdata-stage
  fi
  mkdir -p "$stage"
  local entry base
  while IFS= read -r entry; do
    base=$(basename "$entry")
    case "$base" in
      # plugins/ would make Jellyfin load the real instance's plugins while
      # FERROFIN_DISABLE_EXTENSIONS=1 muzzles Ferrofin's — strip it for BOTH.
      # log/temp/transcodes are runtime junk; the db is real-copied below.
      plugins|log|temp|transcodes|MANIFEST.json) continue;;
    esac
    cp -al "$entry" "$stage/" 2>/dev/null || {
      # Cross-device fallback (target/ on another filesystem): the failed
      # cp -al leaves a read-only directory skeleton plain cp -a cannot write
      # into — free the DIRS (never file modes: hardlinks share the pin's
      # inode), then copy for real. -f unlinks any half-made read-only file;
      # either way a fresh inode, the pin untouched.
      find "$stage/$(basename "$entry")" -type d -exec chmod u+w {} + 2>/dev/null || true
      cp -af "$entry" "$stage/"
    }
  done < <(find "$SUITE_TESTDATA_PIN" -mindepth 1 -maxdepth 1)
  # Directories came over read-only from the pin (cp -al copies dirs for real,
  # hardlinks only files). Make DIRECTORIES writable — and ONLY directories:
  # chmod on a hardlinked file writes through to the pin's shared inode and
  # would strip the pin's own read-only protection.
  find "$stage" -type d -exec chmod u+w {} +
  rm -f -- "$stage/data/jellyfin.db"            # remove the HARDLINK before real-copying
  cp -- "$SUITE_TESTDATA_PIN/data/jellyfin.db" "$stage/data/jellyfin.db"
  chmod u+w -- "$stage/data/jellyfin.db"        # its own inode — safe to make writable
  suite_export_testdata_user
  suite_stage_set_password
  cp -- "$SUITE_TESTDATA_PIN/MANIFEST.json" "$stage/MANIFEST.json"   # stamp LAST: an interrupted stage never matches
}

# Reset the bench user's password IN THE STAGE db (its own inode, never the
# pin) to BENCH_ADMIN_PASSWORD — Jellyfin's own hash format, byte-compatible
# with Ferrofin. Idempotent; also run on stage REUSE, since the configured
# password may have changed since the stage was built.
suite_stage_set_password() {
  python3 - "$SUITE_TESTDATA_STAGE/data/jellyfin.db" "$BENCH_ADMIN_USER" "$BENCH_ADMIN_PASSWORD" <<'PY'
import hashlib, secrets, sqlite3, sys
db, user, pw = sys.argv[1:4]
salt = secrets.token_bytes(16)                      # Jellyfin Constants: 128-bit salt
h = hashlib.pbkdf2_hmac("sha512", pw.encode(), salt, 210000)  # DefaultIterations
cur = sqlite3.connect(f"file:{db}?mode=rw", uri=True).cursor()  # missing db must FAIL, not be created
cur.execute("UPDATE Users SET Password=? WHERE Username=?",
            (f"$PBKDF2-SHA512$iterations=210000${salt.hex().upper()}${h.hex().upper()}", user))
cur.connection.commit()
sys.exit(0 if cur.rowcount == 1 else 1)
PY
}

# The bench user is the snapshot's first user (its real admin) — exported so
# benchlib's AuthenticateByName logs in as a user that actually exists there.
suite_export_testdata_user() {
  BENCH_ADMIN_USER=$(sqlite3 "file:$SUITE_TESTDATA_PIN/data/jellyfin.db?mode=ro&immutable=1" \
                     "SELECT Username FROM Users ORDER BY rowid LIMIT 1")
  [ -n "$BENCH_ADMIN_USER" ] || { echo "testdata pin has no users" >&2; exit 1; }
  export BENCH_ADMIN_USER
}

# Fill one service's config volume from the stage. The volume must exist and
# the container must NOT be running yet (compose create → seed → start).
suite_seed_config_volume() {  # $1 = ferrofin|jellyfin
  local vol="${COMPOSE_PROJECT_NAME:-ferrofin-bench}_$1-config"
  local stage_abs; stage_abs=$(cd "$SUITE_TESTDATA_STAGE" && pwd)
  docker run --rm -v "$vol":/config -v "$stage_abs":/src:ro alpine \
    sh -c 'rm -rf /config/..?* /config/.[!.]* /config/* 2>/dev/null; cp -a /src/. /config/ && rm -f /config/MANIFEST.json && chmod -R u+w /config'
}

# The one bring-up entry: replaces bare `docker compose up -d` at every server
# start site. Testdata mode seeds each server's config volume between create
# and start; otherwise it is exactly the old `up`. Always follow with
# suite_assert_running_mounts_readonly at the call site.
suite_up_seeded() {  # usage: suite_up_seeded [--build] svc...
  local build=""
  [ "${1:-}" = "--build" ] && { build="--build"; shift; }
  if [ "${BENCH_TESTDATA:-0}" = 1 ]; then
    suite_stage_testdata
    # Seed only volumes that do not exist yet: a surviving volume is KEPT data
    # (BENCH_KEEP_DATA, a mid-sweep recreate) and re-seeding it would wipe the
    # adopted state the caller deliberately preserved. (Existing is not proof
    # of seeded — an interrupt between create and seed leaves an empty volume,
    # which then fails loudly downstream as "serves no items".)
    local proj="${COMPOSE_PROJECT_NAME:-ferrofin-bench}" svc seed=()
    for svc in "$@"; do
      case "$svc" in ferrofin|jellyfin)
        docker volume inspect "${proj}_${svc}-config" >/dev/null 2>&1 || seed+=("$svc");;
      esac
    done
    # shellcheck disable=SC2086
    docker compose create $build "$@"
    for svc in ${seed+"${seed[@]}"}; do suite_seed_config_volume "$svc"; done
    docker compose start "$@"
  else
    # shellcheck disable=SC2086
    docker compose up -d $build "$@"
  fi
}
