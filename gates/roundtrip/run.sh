#!/usr/bin/env bash
# gates/roundtrip/run.sh [--image IMAGE] [--jellyfin-image IMAGE] [--fixtures DIR] [--only ROUTE] [--run-id ID] [--work DIR]
#
# The way back to Jellyfin, proved on real Jellyfin: for each route in routes.sh, on a FRESH copy of a
# pristine synthetic fixture (gates/adoption/build-fixtures.sh), each stage records each of its checks:
#   Ferrofin   IMAGE (default ferrofin:bench) adopts the copy and scans it, the usage stage runs (usage.py:
#              watch state, accounts, a device, an API key, display preferences, playlists, collections,
#              libraries, a metadata edit, an image and deletes; README.md) and captures
#              Ferrofin's answers (answers.py) once its closing library scan has completed, and Ferrofin
#              is stopped. Checks: ready; boot log (generation, migrations, no ERROR); the scan completes;
#              the usage stage; a clean stop.
#   Jellyfin   the Jellyfin release UPSTREAM_TAG names (jellyfin/jellyfin:<minor>, or --jellyfin-image)
#              boots on a copy of what Ferrofin left, with no export step yet. Checks: ready within
#              ROUNDTRIP_READY_TIMEOUT; every migration it began finished (a failure names the migration
#              and its SQLite error); no ERR or FTL entry outside checks_boot.py's allowlist up to that
#              point; its answers equal Ferrofin's (adoption_normalise: ids, image sizes and the probes
#              that differ by design left out), so everything answers.py captures came back: users and
#              their logins, watch state, devices, API keys, settings, playlists and collections, and
#              more; integrity_check and foreign_key_check
#              clean once it has stopped.
# The errors check reads the log up to the probes: an error a probe causes is judged by the answers.
# One PASS/FAIL line per route; the exit status is 1 if any failed, 2 if the run was refused. Every
# check's verdict goes to WORK/results.jsonl as it runs, and WORK/report.html is built from it however
# the run ends (report.py). A failed route keeps its copies under WORK; a passed one keeps only its logs
# and answers. The stages are stage_ferrofin.sh and stage_jellyfin.sh.
#
# Several runs can share one host: a run has an id (--run-id, default <time>-<pid>), and its containers
# (rt-<id>-<route>-<server>), the tag it runs IMAGE as (ferrofin-roundtrip:<id>) and its work directory
# are named for it, and an id another run holds is refused. Ports are the ones docker picks; there is no
# lock, and a run removes only what it made (its containers by the ids in its own cidfiles).
# It never builds or pulls an image. Fixtures are synthetic: never point it at a real library or a
# network share (refused). DIR defaults to the one build-fixtures.sh builds under $FERROFIN_GATES_DIR; the
# work directory to $FERROFIN_GATES_DIR/runs/roundtrip-<id>, never reused.
# Settings (environment): ROUNDTRIP_READY_TIMEOUT, the seconds a server may take to answer after its
# start (default 300); ROUNDTRIP_SCAN_TIMEOUT, the seconds a library scan or a settle may take (default
# 600, as the adoption gate's); ROUNDTRIP_STOP_TIMEOUT, the seconds a TERM is given before docker kills
# the server (default 60); ROUNDTRIP_DOCKER_TIMEOUT, the seconds any other docker call may take (default 120).
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
if [ -n "$ONLY" ]; then
  printf '%s\n' "${ROUNDTRIP_ROUTES[@]}" | cut -d'|' -f1 | grep -qxF -- "$ONLY" || die "no route $ONLY in routes.sh"
fi
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
mark() {
  local detail='' verdict=PASS i
  for ((i = mark_from; i < ${#why[@]}; i++)); do detail+="${detail:+; }${why[i]}"; verdict=FAIL; done
  mark_from=${#why[@]}
  record_detail check "$WORK" "$route" "$1" "$2" "$verdict" "$detail"
}
# check <reason-producing command…>: add each line it prints to why[].
check() { mapfile -t -O "${#why[@]}" why < <("$@"); }
# reasons <command…>: the command's output as reasons when it fails, nothing when it succeeds.
# shellcheck disable=SC2317,SC2329  # called through check (SC2317 is the code before shellcheck 0.11)
reasons() { local out; out=$("$@" 2>&1) || printf '%s\n' "${out:-$1 failed}"; }
# shellcheck source=gates/roundtrip/stage_ferrofin.sh
. "$HERE/stage_ferrofin.sh"   # ferrofin_stage
# shellcheck source=gates/roundtrip/stage_jellyfin.sh
. "$HERE/stage_jellyfin.sh"   # jellyfin_stage


MEDIA=(-v "$FIXTURES/media:/media:ro")
failed=0
for spec in "${ROUNDTRIP_ROUTES[@]}"; do
  IFS='|' read -r route fixture generation <<<"$spec"
  [ -z "$ONLY" ] || [ "$ONLY" = "$route" ] || continue
  dst=$WORK/$route
  if [ ! -d "$FIXTURES/$fixture" ]; then
    printf '%-5s %-14s %s\n' FAIL "$route" "fixture $fixture missing: run gates/adoption/build-fixtures.sh"
    record_detail row "$WORK" "$route" path "$generation" FAIL "fixture $fixture missing"; failed=1; continue
  fi
  mkdir -p "$dst"
  record begin "$WORK" "$route" path "$generation"
  why=(); mark_from=0
  ferrofin_stage && jellyfin_stage
  verdict=PASS; [ "${#why[@]}" = 0 ] || { verdict=FAIL; failed=1; }
  detail=$(printf '%s; ' "${why[@]}"); detail=${detail%; }
  printf '%-5s %-14s %s\n' "$verdict" "$route" "$detail"
  record_detail row "$WORK" "$route" path "$generation" "$verdict" "$detail"
  # a passed route keeps its logs and answers; its copies go (a failed one keeps them to look into)
  [ "$verdict" = FAIL ] || rm -rf -- "${dst:?}/ferrofin" "${dst:?}/ferrofin-cache" "${dst:?}/jellyfin" "${dst:?}/jellyfin-cache"
done
exit "$failed"
