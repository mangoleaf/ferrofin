#!/usr/bin/env bash
# gates/roundtrip/run.sh [--image IMAGE] [--jellyfin-image IMAGE] [--fixtures DIR] [--only ROUTE[,ROUTE…]] [--run-id ID] [--work DIR]
#
# The way back to Jellyfin, proved on real Jellyfin: for each route in routes.sh, on a FRESH copy of a
# pristine synthetic fixture (gates/adoption/build-fixtures.sh), each stage records each of its checks:
#   Ferrofin   IMAGE (default ferrofin:bench) adopts the copy and scans it, the usage stage runs (usage.py:
#              watch state, accounts, a device, an API key, display preferences, playlists, collections,
#              libraries, a metadata edit, an image, deletes and a removed share; README.md) and captures
#              Ferrofin's answers (answers.py) once its closing library scan has completed, and Ferrofin
#              is stopped. Checks: ready; boot log (generation, migrations, no ERROR); the scan completes;
#              the usage stage; a clean stop.
#   Export     `ferrofin-server export-jellyfin` from IMAGE, in a container of its own, writes what
#              Ferrofin left into the copy Jellyfin boots on (stage_export.sh). Checks: it completes; every
#              path the exported database stores resolves in it, in the layout of Jellyfin's image, and
#              none lies under the directory Ferrofin ran at (checks_paths.py stored).
#   Jellyfin   the Jellyfin release UPSTREAM_TAG names (jellyfin/jellyfin:<minor>, or --jellyfin-image)
#              boots on the export. Checks: ready within ROUNDTRIP_READY_TIMEOUT; every migration it
#              began finished (a failure names the migration and its SQLite error); it ran the code
#              migrations its own first start on the route's source ran (routes.sh), but those the export
#              records as applied, and no schema migration; no ERR or FTL entry outside checks_boot.py's allowlist up to that
#              point; a full metadata refresh of every playlist completes (api.py refresh-playlists),
#              then its answers equal Ferrofin's (adoption_normalise: ids, image sizes and the probes
#              that differ by design left out), so everything answers.py captures came back: users and
#              their logins, watch state, devices, API keys, settings, playlists and collections, and
#              more; it serves every image, chapter and profile image, trickplay tile, external subtitle
#              and attachment its database lists (probes.py, which writes probes.txt); its library scan
#              completes; integrity_check and foreign_key_check clean once it has stopped; and every
#              library folder row Ferrofin stopped with is still there after that scan, with its
#              DateCreated (checks_paths.py libraries).
# The errors check reads the log up to the probes: an error a probe causes is judged by the answers.
# Each route is a record in routes.sh: its fixture, the directories Ferrofin runs in, the media tree it mounts
# and its kind (a scale route's time and size are recorded on its line, not gated). One PASS/FAIL line per
# route listed (a missing fixture is a FAIL line too, never a skip), then how many passed; the exit status is
# 1 if any failed, 2 if the run was refused (an unknown route in --only, or no route to run). Every check's verdict goes to WORK/results.jsonl
# as it runs, and WORK/report.html and WORK/result.json (the gates' envelope, gates/result.py, which a
# gates/run session reads) are built from it however the run ends (report.py). A failed route keeps its
# copies under WORK; a passed one keeps only its logs and answers. The stages are stage_ferrofin.sh,
# stage_export.sh and stage_jellyfin.sh.
#
# Several runs can share one host: a run has an id (--run-id, default <time>-<pid>), and its containers
# (rt-<id>-<route>-<server>), the tag it runs IMAGE as (ferrofin-roundtrip:<id>) and its work directory
# are named for it, and an id another run holds is refused. Ports are the ones docker picks; there is no
# lock, and a run removes only what it made (its containers by the ids in its own cidfiles).
# It never builds or pulls an image. Fixtures are synthetic: never point it at a real library or a
# network share (refused). DIR defaults to the one build-fixtures.sh builds under $FERROFIN_GATES_DIR; the
# work directory to $FERROFIN_GATES_DIR/runs/roundtrip-<id>, never reused.
# Settings (environment): ROUNDTRIP_READY_TIMEOUT, the seconds a server may take to answer after its
# start (default 300); ROUNDTRIP_SCAN_TIMEOUT, the seconds a library scan, a settle, the export or the
# playlists' refresh may take (default 600, as the adoption gate's); ROUNDTRIP_STOP_TIMEOUT, the seconds a
# TERM is given before docker kills the server (default 60); ROUNDTRIP_DOCKER_TIMEOUT, the seconds any other
# docker call may take (default 120).
set -uo pipefail
CALLER=$PWD
SELF=$(cd "$(dirname "$0")" && pwd)/${0##*/}
cd "$(dirname "$0")/../.." || exit 2
# shellcheck source=gates/roundtrip/lib.sh
. gates/roundtrip/lib.sh
# shellcheck source=gates/roundtrip/routes.sh
. gates/roundtrip/routes.sh
gates_supervise "$CALLER" "$0" "$@"   # the run itself goes to a worker (gates/lib.sh, gl-968)

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$SELF"; }
die() { echo "roundtrip: $*" >&2; exit 2; }
HERE=gates/roundtrip
IMAGE=ferrofin:bench JELLYFIN_IMAGE='' FIXTURES=${FERROFIN_ADOPTION_FIXTURES:-} ONLY='' RUN_ID='' WORK=''
abs() { case $1 in /*) printf '%s\n' "$1";; *) printf '%s/%s\n' "$CALLER" "$1";; esac; }
while [ $# -gt 0 ]; do
  case $1 in
    --image|--jellyfin-image|--fixtures|--only|--run-id|--work) [ $# -ge 2 ] || die "$1 needs a value";;
  esac
  case $1 in
    --image) IMAGE=$2; shift 2;;
    --jellyfin-image) JELLYFIN_IMAGE=$2; shift 2;;
    --fixtures) FIXTURES=$(abs "$2"); shift 2;;
    --only) ONLY=$2; shift 2;;
    --run-id) RUN_ID=$2; shift 2;;
    --work) WORK=$(abs "$2"); shift 2;;
    -h|--help) usage; exit 0;;
    *) die "unknown argument $1";;
  esac
done
READY_TIMEOUT=${ROUNDTRIP_READY_TIMEOUT:-300}
SCAN_TIMEOUT=${ROUNDTRIP_SCAN_TIMEOUT:-600}
STOP_TIMEOUT=${ROUNDTRIP_STOP_TIMEOUT:-60}
for setting in READY_TIMEOUT SCAN_TIMEOUT STOP_TIMEOUT; do
  [[ "${!setting}" =~ ^[1-9][0-9]*$ ]] || die "ROUNDTRIP_$setting must be a positive integer"
done
[[ "$ROUNDTRIP_DOCKER_TIMEOUT" =~ ^[1-9][0-9]*$ ]] || die "ROUNDTRIP_DOCKER_TIMEOUT must be a positive integer"
for t in docker sqlite3 jq curl python3 timeout; do command -v "$t" >/dev/null || die "$t not installed"; done
RUN_ID=${RUN_ID:-$(rt_new_run_id)}
rt_valid_run_id "$RUN_ID" || die "--run-id must be lower-case letters, digits, dots and dashes (at most 48), not '$RUN_ID'"
IFS=, read -ra WANTED <<<"$ONLY"
for want in "${WANTED[@]}"; do
  printf '%s\n' "${ROUNDTRIP_ROUTES[@]}" | cut -d'|' -f1 | grep -qxF -- "$want" || die "no route $want in routes.sh"
done
SELECTED=()
for spec in "${ROUNDTRIP_ROUTES[@]}"; do   # the fields routes.sh describes
  IFS='|' read -r route _ _ _ ran_at config_at kind <<<"$spec"
  [[ $ran_at == /?* ]] || die "route $route in routes.sh names no absolute data directory for Ferrofin"
  [[ $config_at == - || $config_at == /?* ]] || die "route $route in routes.sh names a configuration directory that is neither - nor absolute"
  [[ $kind =~ ^(path|messy|scale)$ ]] || die "route $route in routes.sh is of kind '$kind', not path, messy or scale"
  [ -z "$ONLY" ] || [[ ",$ONLY," == *",$route,"* ]] && SELECTED+=("$spec")
done
(( ${#SELECTED[@]} )) || die "no route to run: routes.sh lists none"
[ -n "$FIXTURES" ] || FIXTURES=$(adoption_fixtures_dir roundtrip) || exit 2
[ -d "$FIXTURES/media" ] || die "$FIXTURES has no media/: run gates/adoption/build-fixtures.sh first"
if [ -z "$WORK" ]; then WORK=$(gates_dir roundtrip "runs/roundtrip-$RUN_ID") || exit 2; fi
gates_require_local "$FIXTURES" && gates_require_local "$WORK" || exit 2
[ ! -e "$WORK" ] || die "$WORK exists: a run's work directory is never reused"
[ -n "$JELLYFIN_IMAGE" ] || JELLYFIN_IMAGE=$(rt_jellyfin_image crates/ferrofin-api/src/handlers.rs) || exit 2
rt_docker image inspect "$IMAGE" >/dev/null 2>&1 || die "image $IMAGE missing (docker build -t $IMAGE .)"
rt_docker image inspect "$JELLYFIN_IMAGE" >/dev/null 2>&1 || die "image $JELLYFIN_IMAGE missing (docker pull $JELLYFIN_IMAGE)"
IMAGE_ID=$(rt_docker image inspect -f '{{.Id}}' "$IMAGE")
COMMIT=$(rt_docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$IMAGE")
JELLYFIN_ID=$(rt_docker image inspect -f '{{.Id}}' "$JELLYFIN_IMAGE")   # run by id: a pull mid-run changes nothing
TAG=$(rt_image_tag "$RUN_ID")
! rt_docker image inspect "$TAG" >/dev/null 2>&1 || die "the tag $TAG exists: another run has the id $RUN_ID"
# smoke.sh probes as the account the fixture's oracle ran as, as the adoption gate does
[ -n "${ADOPTION_USER:-}" ] || [ ! -f "$FIXTURES/oracle/user.txt" ] || ADOPTION_USER=$(cat "$FIXTURES/oracle/user.txt")
export ADOPTION_USER
# claimed with a plain mkdir: of two runs given one directory, only one gets it
{ mkdir -p "$(dirname "$WORK")" && mkdir "$WORK"; } 2>/dev/null || die "cannot create $WORK (a run's work directory is never reused)"
MADE=()   # the cidfile of every container this run started, removed however it ends
TAGGED=""
REPORT_PY=gates/adoption/adoption_report.py
record() { python3 "$REPORT_PY" "$@" >/dev/null 2>&1 || true; }
record_detail() { local detail=${!#}; set -- "${@:1:$#-1}"; printf '%s' "$detail" | python3 "$REPORT_PY" "$@" --detail-stdin >/dev/null 2>&1 || true; }
# shellcheck disable=SC2317,SC2329  # invoked by the EXIT trap
cleanup() {
  local rc=$? cid
  trap '' INT TERM HUP
  gates_ack
  for cid in "${MADE[@]}"; do rt_remove "$cid"; done
  [ -z "$TAGGED" ] || rt_docker rmi "$TAG" >/dev/null 2>&1   # the tag only: the image keeps its other names
  if report=$(python3 "$HERE/report.py" build "$WORK" 2>&1); then echo "report: $report"; else echo "report: $report" >&2; fi
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
rt_docker tag "$IMAGE_ID" "$TAG" || die "cannot tag $IMAGE as $TAG"
TAGGED=1
echo "roundtrip: run $RUN_ID, $IMAGE ($IMAGE_ID) to $JELLYFIN_IMAGE ($JELLYFIN_ID), work $WORK"
record start "$WORK" --title "gates/roundtrip/run.sh" --image "$IMAGE" --oracle "$JELLYFIN_IMAGE ($JELLYFIN_ID)" \
  --image-id "$IMAGE_ID" --commit "$COMMIT"

# mark <stage> <check>: record the verdict of the check that just ran, from the reasons it added to why[].
# shellcheck disable=SC2317,SC2329  # called by the stages, which the loop runs through timed (SC2317 before shellcheck 0.11)
mark() {
  local detail='' verdict=PASS i
  for ((i = mark_from; i < ${#why[@]}; i++)); do detail+="${detail:+; }${why[i]}"; verdict=FAIL; done
  mark_from=${#why[@]}
  record_detail check "$WORK" "$route" "$1" "$2" "$verdict" "$detail"
}
# check <reason-producing command…>: add each line it prints to why[].
# shellcheck disable=SC2317,SC2329  # called by the stages (SC2317 before shellcheck 0.11)
check() { mapfile -t -O "${#why[@]}" why < <("$@"); }
# reasons <command…>: the command's output as reasons when it fails, nothing when it succeeds.
# shellcheck disable=SC2317,SC2329  # called through check (SC2317 is the code before shellcheck 0.11)
reasons() { local out; out=$("$@" 2>&1) || printf '%s\n' "${out:-$1 failed}"; }
# shellcheck source=gates/roundtrip/stage_ferrofin.sh
. "$HERE/stage_ferrofin.sh"   # ferrofin_stage
# shellcheck source=gates/roundtrip/stage_export.sh
. "$HERE/stage_export.sh"     # export_stage
# shellcheck source=gates/roundtrip/stage_jellyfin.sh
. "$HERE/stage_jellyfin.sh"   # jellyfin_stage


# timed <stage function>: run it, adding its wall time to took[].
timed() { local from=$SECONDS rc=0; "$@" || rc=$?; took+=("$((SECONDS - from)) s"); return "$rc"; }
failed=0 passed=0
for spec in "${SELECTED[@]}"; do
  IFS='|' read -r route fixture generation first_start ran_at config_at kind <<<"$spec"
  dst=$WORK/$route
  case $kind in path) media=media;; *) media=media-$kind;; esac
  MEDIA=(-v "$FIXTURES/$media:/media:ro")
  # where Ferrofin runs, for its server and its export: the data directory, and the configuration apart from it
  SERVER_DIRS=(-e FERROFIN_DATA_DIR="$ran_at" -v "$dst/ferrofin:$ran_at")
  [ "$config_at" = - ] || SERVER_DIRS+=(-e FERROFIN_CONFIG_DIR="$config_at" -v "$dst/ferrofin-config:$config_at")
  for needed in "$fixture" "$first_start.server.log" "$media"; do
    [ ! -e "$FIXTURES/$needed" ] || continue
    printf '%-5s %-14s %s\n' FAIL "$route" "fixture $needed missing: build the fixtures again into a new directory (gates/adoption/build-fixtures.sh)"
    record_detail row "$WORK" "$route" "$kind" "$generation" FAIL "fixture $needed missing"; failed=1; continue 2
  done
  mkdir -p "$dst"
  record begin "$WORK" "$route" "$kind" "$generation"
  why=(); mark_from=0; took=(); size=''
  if timed ferrofin_stage && timed export_stage; then
    [ "$kind" != scale ] || size=$(du -sh "$dst/jellyfin" 2>/dev/null | cut -f1)   # as written, before Jellyfin's start
    timed jellyfin_stage
  fi
  verdict=PASS; [ "${#why[@]}" = 0 ] || { verdict=FAIL; failed=1; }
  detail=$(printf '%s; ' "${why[@]}"); detail=${detail%; }
  if [ "$kind" = scale ]; then   # recorded, not gated
    detail+="${detail:+; }took: Ferrofin ${took[0]:-not run}, export ${took[1]:-not run}, Jellyfin ${took[2]:-not run}; the export ${size:-not written}"
  fi
  printf '%-5s %-14s %s\n' "$verdict" "$route" "$detail"
  record_detail row "$WORK" "$route" "$kind" "$generation" "$verdict" "$detail"
  [ "$verdict" = FAIL ] || passed=$((passed + 1))
  # a passed route keeps its logs and answers; its copies go (a failed one keeps them to look into)
  [ "$verdict" = FAIL ] || rm -rf -- "${dst:?}/ferrofin" "${dst:?}/ferrofin-config" "${dst:?}/ferrofin-cache" "${dst:?}/jellyfin" "${dst:?}/jellyfin-cache"
done
echo "roundtrip: $passed of ${#SELECTED[@]} route(s) passed"
exit "$failed"
