#!/usr/bin/env bash
# Functions shared by gates/roundtrip/run.sh and its tests. Sourced, never executed. Docker is reached
# only through rt_docker, so gates/roundtrip/tests/*.bats drive everything with a fake docker on PATH.
#
# Concurrent runs (several agents run this gate at once on one host): everything a run creates is named
# for its run id, a container (rt_name), the image tag it runs (rt_image_tag) and its work directory, and
# every port is one docker allocated when the container started (rt_start). There is no lock and no fixed
# port, and a run only ever removes what it created: a container by the id docker wrote to the run's own
# cidfile (rt_remove), never by a name another run could hold.
# shellcheck source=gates/adoption/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/../adoption/lib.sh"   # gates/lib.sh, adoption_normalise, adoption_check_db, …

# Seconds one docker call may take (ROUNDTRIP_DOCKER_TIMEOUT): a wedged daemon must not hold a run forever.
ROUNDTRIP_DOCKER_TIMEOUT=${ROUNDTRIP_DOCKER_TIMEOUT:-120}
# Seconds a bounded command is given after its TERM before it is killed.
RT_KILL_AFTER=5
rt_docker() { timeout --kill-after="$RT_KILL_AFTER" "$ROUNDTRIP_DOCKER_TIMEOUT" docker "$@"; }

# reasons_within <seconds> <what> <command…>: the command's output as reasons when it fails, nothing when
# it succeeds, and a reason when it was stopped once <seconds> had passed (a server that takes a request
# and never answers must not hold the run). timeout runs in a process group of its own, so a TERM to the
# run does not reach the command: it goes on against a container the run's cleanup has removed, where
# every request is refused at once, and ends within seconds.
reasons_within() {
  local seconds=$1 what=$2 out rc=0; shift 2
  out=$(timeout --kill-after="$RT_KILL_AFTER" "$seconds" "$@" 2>&1) || rc=$?
  case $rc in
    0) ;;
    124|137) printf '%s\n' "${out:+$out; }$what had no result within $seconds s";;
    *) printf '%s\n' "${out:-$what failed}";;
  esac
}

# rt_new_run_id: a run id no other live run on this host has: the start time and this shell's PID.
rt_new_run_id() { printf '%s-%s\n' "$(date +%Y%m%d-%H%M%S)" "$$"; }

# rt_valid_run_id <id>: whether <id> can name containers, an image tag and a directory.
rt_valid_run_id() { [[ $1 =~ ^[a-z0-9][a-z0-9.-]{0,47}$ ]]; }

# rt_name <run id> <route> <role>: the container of one server of one route of one run.
rt_name() { printf 'rt-%s-%s-%s\n' "$1" "$2" "$3"; }

# rt_image_tag <run id>: the tag the run gives the Ferrofin image under test, so a rebuild of that image
# by anyone else during the run changes nothing this run starts.
rt_image_tag() { printf 'ferrofin-roundtrip:%s\n' "$1"; }

# rt_jellyfin_image <handlers.rs>: the Jellyfin release the round trip returns to, the minor release of
# UPSTREAM_TAG (v12.4 → jellyfin/jellyfin:12.4). A refusal is one line on stderr, and status 2.
rt_jellyfin_image() {
  local tag
  tag=$(sed -n 's/^pub const UPSTREAM_TAG: &str = "v\([0-9][0-9]*\.[0-9][0-9]*\)[^"]*";$/\1/p' "$1" 2>/dev/null)
  [ -n "$tag" ] || { echo "cannot read UPSTREAM_TAG from $1" >&2; return 2; }
  printf 'jellyfin/jellyfin:%s\n' "$tag"
}

# rt_start <name> <cidfile> <image> <docker run arguments…>: start a server container as the calling user,
# its id in <cidfile> (which must not exist; docker writes it only for a container it created), its 8096
# published on a loopback port docker picks, its outbound HTTP(S) sent to an unreachable proxy (as the
# adoption gate does: no provider or plugin repository is reached). Prints that port.
rt_start() {
  local name=$1 cidfile=$2 image=$3 published; shift 3
  rt_docker run -d --pull never --cidfile "$cidfile" --name "$name" --user "$(id -u):$(id -g)" -p 127.0.0.1::8096 \
    -e HTTP_PROXY=http://127.0.0.1:9 -e HTTPS_PROXY=http://127.0.0.1:9 \
    -e ALL_PROXY=http://127.0.0.1:9 -e NO_PROXY=127.0.0.1,localhost "$@" "$image" >/dev/null || return 1
  published=$(rt_docker port "$name" 8096/tcp 2>&1)
  [[ ${published%%$'\n'*} =~ :([0-9]+)$ ]] ||
    { echo "docker published no port for $name (a container that exited at once has none): ${published:-nothing}" >&2; return 1; }
  printf '%s\n' "${BASH_REMATCH[1]}"
}

# rt_wait_up <name> <port> <seconds> [give-up ERE]: the server answers, or status 1 as soon as its
# container has exited (a server that refuses its database exits at once), its log matches the give-up
# pattern (Jellyfin stays up after a failed start, answering 503) or once <seconds> have passed.
rt_wait_up() {
  local name=$1 port=$2 deadline=$((SECONDS + $3)) give_up=${4:-} log
  while [ "$SECONDS" -lt "$deadline" ]; do
    curl -sf --max-time 5 "http://127.0.0.1:$port/System/Info/Public" >/dev/null && return 0
    [ "$(rt_docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null)" = true ] || return 1
    if [ -n "$give_up" ]; then
      log=$(rt_docker logs "$name" 2>&1)   # whole, not piped: grep -q's early exit would fail the pipe
      ! grep -qE -- "$give_up" <<<"$log" || return 1
    fi
    sleep 2
  done
  return 1
}

# What Jellyfin logs when its start failed (Jellyfin.Server/Program.cs, LogCritical): it keeps serving
# 503 from its setup server, so the wait gives up on this line instead of waiting out its deadline.
# shellcheck disable=SC2034  # read by run.sh
RT_JELLYFIN_START_FAILED='^\[[0-9:]+\] \[FTL\] \[[0-9]+\] Main: Error while starting server'

# rt_stop <name> <seconds>: stop the server as `docker stop` does (TERM, then KILL after <seconds>); a
# reason when it did not exit by itself with status 0. The docker call may take <seconds> more than any other.
rt_stop() {
  local code
  ROUNDTRIP_DOCKER_TIMEOUT=$((ROUNDTRIP_DOCKER_TIMEOUT + $2)) rt_docker stop -t "$2" "$1" >/dev/null 2>&1 ||
    { echo "docker stop failed"; return 0; }
  code=$(rt_docker inspect -f '{{.State.ExitCode}}' "$1" 2>/dev/null)
  [ "$code" = 0 ] || echo "exited with status ${code:-unknown} after TERM (137: killed after ${2} s)"
}

# rt_not_up <name> <seconds>: why a server never answered: it exited (with its status), or the deadline.
rt_not_up() {
  local running
  running=$(rt_docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null) ||
    { echo "never became ready within $2 s (and docker inspect could not tell whether it runs)"; return 0; }
  if [ "$running" = true ]; then
    echo "never became ready within $2 s"
  else
    echo "exited with status $(rt_docker inspect -f '{{.State.ExitCode}}' "$1" 2>/dev/null || echo unknown) before it answered"
  fi
}

# rt_remove <cidfile>: remove the container docker created with that cidfile, if it did, and the file.
rt_remove() {
  [ -s "$1" ] || return 0
  rt_docker rm -f "$(cat "$1")" >/dev/null 2>&1 && rm -f -- "$1"
}

# rt_compare_answers <oracle> <answers>: a reason when Jellyfin's answers differ from Ferrofin's,
# both normalised as the adoption gate does (ids, image sizes and the probes that differ by design), or
# when the user data does with its item ids kept: both servers read one database, so the ids name the
# same items, and a watch state moved to another item must not compare equal.
rt_compare_answers() {
  local diff
  [ -s "$2" ] || { echo "no answers from Jellyfin"; return 0; }
  diff=$(diff <(adoption_normalise "$1") <(adoption_normalise "$2")) || true
  [ -n "$diff" ] || diff=$(diff <(grep -E '^user(data|plays) ' "$1") <(grep -E '^user(data|plays) ' "$2")) || true
  [ -n "$diff" ] || return 0
  printf '%s line(s) differ, first: %s (diff %s %s)\n' "$(grep -c '^[<>]' <<<"$diff")" \
    "$(grep -m1 '^[<>]' <<<"$diff" | cut -c1-160)" "$1" "$2"
}
