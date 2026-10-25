#!/usr/bin/env bash
# gates/verify/run.sh [--image IMAGE] [--out DIR] [--clip FILE]
#
# The scan-behaviour check (scan-behaviour.sh, README.md) against a fresh Ferrofin started from IMAGE
# (default ferrofin:bench) that holds nothing of anyone else's: a new data directory and two empty
# libraries under DIR, a movie library and a TV library (row 13 needs one), /metrics on, real-time
# monitoring and every metadata fetcher off, and its outbound HTTP(S) sent to an unreachable proxy.
# The check runs on each library in turn. DIR defaults to
# $FERROFIN_GATES_DIR/runs/verify-<YYYYMMDD-HHMM> (gates/lib.sh) and is never reused. It ends with
#   DIR/scan-behaviour-movies.txt, DIR/scan-behaviour-tvshows.txt   the check's rows on each
#   DIR/verify.json      the same rows as data, per library (null when the check ended before them),
#                        and the exit status: what gates/report.py reads
#   DIR/server.log                                                  the server's log
# and the container (gates-verify) removed, however the run ends. --clip is passed to the check.
# Settings (environment): VERIFY_PORT, the loopback port the server answers on (default 18131);
# READY_TIMEOUT_S, how long the server may take to answer (default 300), and HTTP_TIMEOUT_S, how long
# one request or docker call may take (default 10), as gates/bench/run.sh has them.
# Exit: 0 nothing failed; 1 a row failed, or the server never answered or could not be set up; 2
# refused before anything started; 3 nothing could be verified on a library (the check's own codes).
set -uo pipefail
CALLER=$PWD
SELF=$(cd "$(dirname "$0")" && pwd)/${0##*/}   # for usage, after the cd below
cd "$(dirname "$0")/../.." || exit 2
# shellcheck source=gates/lib.sh
. gates/lib.sh
# shellcheck source=gates/verify/lib.sh
. gates/verify/lib.sh   # verify_combine

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$SELF"; }
die() { echo "verify: $*" >&2; exit 2; }
IMAGE=ferrofin:bench; OUT=; CLIP=()
while [ $# -gt 0 ]; do
  case $1 in
    --image|--out|--clip) [ $# -ge 2 ] || die "$1 needs a value";;
  esac
  case $1 in
    --image) IMAGE=$2; shift 2;;
    --out) case $2 in /*) OUT=$2;; *) OUT=$CALLER/$2;; esac; shift 2;;
    --clip) case $2 in /*) CLIP=(--clip "$2");; *) CLIP=(--clip "$CALLER/$2");; esac; shift 2;;
    -h|--help) usage; exit 0;;
    *) die "unknown argument $1";;
  esac
done
VERIFY_PORT=${VERIFY_PORT:-18131}
READY_TIMEOUT_S=${READY_TIMEOUT_S:-300}
HTTP_TIMEOUT_S=${HTTP_TIMEOUT_S:-10}
for setting in VERIFY_PORT READY_TIMEOUT_S HTTP_TIMEOUT_S; do [[ "${!setting}" =~ ^[1-9][0-9]*$ ]] || die "$setting must be a positive integer"; done
(( VERIFY_PORT <= 65535 )) || die "VERIFY_PORT is at most 65535"
for t in docker curl jq; do command -v "$t" >/dev/null || die "$t not installed"; done
# Every request and docker call is bounded, so a server that takes a connection and never answers
# cannot hold the run (and a gates/run session's lock) forever.
http() { curl -sf --connect-timeout "$HTTP_TIMEOUT_S" --max-time "$HTTP_TIMEOUT_S" "$@"; }
dock() { timeout --kill-after=2 "$HTTP_TIMEOUT_S" docker "$@"; }
if [ -z "$OUT" ]; then
  RUNS=$(gates_dir verify runs) || exit 2
  base=verify-$(date +%Y%m%d-%H%M); OUT=$RUNS/$base; n=1
  while [ -e "$OUT" ]; do n=$((n + 1)); OUT=$RUNS/$base-run$n; done
fi
gates_require_local "$OUT" || exit 2
[ ! -e "$OUT" ] || die "$OUT exists: a run directory is never reused"
dock image inspect "$IMAGE" >/dev/null 2>&1 || die "image $IMAGE missing"
NAME=gates-verify
[ -z "$(dock ps -aq --filter "name=^$NAME\$")" ] || die "a $NAME container exists (another run's); it is not this run's to remove"

mkdir -p "$OUT/data" "$OUT/cache" "$OUT/library/movies/scratch" "$OUT/library/tv/Ferrofin Verify Show" || exit 2
OUT=$(cd -P "$OUT" && pwd)   # the check wants the scratch folder without symlinks
URL=http://127.0.0.1:$VERIFY_PORT
# The admin's password and token stay off every command line (ps shows those to every user): the
# container reads the password from an env file, curl the token from a header file, both 0600.
SECRETS=$(mktemp -d "$OUT/.secrets.XXXXXX") || exit 2   # mktemp makes it 0700
PASSWORD=$(head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9')
(umask 077; printf 'FERROFIN_ADMIN_USER=verify\nFERROFIN_ADMIN_PASSWORD=%s\n' "$PASSWORD" > "$SECRETS/env")
AUTH='MediaBrowser Client="gates", Device="gates", DeviceId="gates-verify", Version="1"'
# shellcheck disable=SC2317,SC2329  # invoked by the EXIT trap
cleanup() {   # only the container this run made (its id in the cidfile): never another run's of the same name
  trap '' INT TERM HUP
  if [ -s "$SECRETS/cid" ]; then
    dock logs "$(cat "$SECRETS/cid")" > "$OUT/server.log" 2>&1
    dock rm -f "$(cat "$SECRETS/cid")" >/dev/null 2>&1
  fi
  rm -f -- "$SECRETS/env" "$SECRETS/auth" "$SECRETS/cid"; rmdir -- "$SECRETS" 2>/dev/null
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
fail() { echo "verify: $*" >&2; exit 1; }

dock run -d --name "$NAME" --cidfile "$SECRETS/cid" --user "$(id -u):$(id -g)" -p "127.0.0.1:$VERIFY_PORT:8096" --env-file "$SECRETS/env" \
  -e FERROFIN_DATA_DIR=/config -e FERROFIN_CACHE_DIR=/cache -e FERROFIN_ENABLE_METRICS=true \
  -e HTTP_PROXY=http://127.0.0.1:9 -e HTTPS_PROXY=http://127.0.0.1:9 -e NO_PROXY=127.0.0.1,localhost \
  -v "$OUT/data:/config" -v "$OUT/cache:/cache" -v "$OUT/library:/media" "$IMAGE" >/dev/null \
  || fail "the server could not be started from $IMAGE"
deadline=$((SECONDS + READY_TIMEOUT_S))
until http "$URL/System/Info/Public" >/dev/null; do
  [ "$(dock inspect -f '{{.State.Running}}' "$(cat "$SECRETS/cid")" 2>/dev/null)" = true ] || fail "the server exited (see $OUT/server.log)"
  (( SECONDS < deadline )) || fail "the server did not answer within ${READY_TIMEOUT_S}s (see $OUT/server.log)"
  sleep 1
done
TOKEN=$(VERIFY_PW=$PASSWORD jq -n '{Username: "verify", Pw: env.VERIFY_PW}' \
  | http -H "Authorization: $AUTH" -H 'Content-Type: application/json' --data-binary @- "$URL/Users/AuthenticateByName" \
  | jq -r '.AccessToken // empty') && [ -n "$TOKEN" ] || fail "the administrator could not sign in"
(umask 077; printf 'Authorization: MediaBrowser Token="%s", Client="gates", Device="gates", DeviceId="gates-verify", Version="1"\n' "$TOKEN" > "$SECRETS/auth")
# library <name> <collection type> <folder> <item type…>: an empty library on /media/<folder>, with
# real-time monitoring off (the check refuses it on) and no metadata fetchers for those item types,
# so its rows expect no provider requests; prints its id.
library() {
  local name=$1 kind=$2 folder=$3; shift 3
  jq -n '{LibraryOptions: {EnableRealtimeMonitor: false,
          TypeOptions: [$ARGS.positional[] | {Type: ., MetadataFetchers: [], ImageFetchers: []}]}}' --args "$@" \
    | http -H "@$SECRETS/auth" -H 'Content-Type: application/json' --data-binary @- \
      "$URL/Library/VirtualFolders?name=$name&collectionType=$kind&paths=%2Fmedia%2F$folder&refreshLibrary=false" >/dev/null \
    || fail "the $kind library could not be created"
  http -H "@$SECRETS/auth" "$URL/Library/VirtualFolders" | jq -er --arg n "$name" '.[] | select(.Name == $n) | .ItemId' \
    || fail "the $kind library is not listed"
}
MOVIES=$(library VerifyMovies movies movies Movie) || exit 1
SHOWS=$(library VerifyShows tvshows tv Series Season Episode) || exit 1
rc=0
for kind in movies tvshows; do
  case $kind in
    movies) id=$MOVIES scratch=movies/scratch;;
    tvshows) id=$SHOWS scratch="tv/Ferrofin Verify Show";;   # the scratch series, named as the check requires
  esac
  echo "== the $kind library"
  FERROFIN_TOKEN=$TOKEN gates/verify/scan-behaviour.sh "${CLIP[@]}" --json "$OUT/rows-$kind.json" \
    "$URL" "$id" "$OUT/library/$scratch" "/media/$scratch" \
    | tee "$OUT/scan-behaviour-$kind.txt"
  this=${PIPESTATUS[0]}
  if [ "$this" = 1 ] || [ "$rc" = 0 ]; then rc=$this; fi   # a failed row outranks anything else
done
verify_combine "$OUT" "$rc" movies tvshows
exit "$rc"
