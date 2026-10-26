#!/usr/bin/env bash
# Shared by the gates' scripts. Sourced, never executed: nothing here touches docker or the
# network, which is what lets gates/tests/gates.bats drive it with throwaway directories.
#
# The output root. Everything a gate creates goes under FERROFIN_GATES_DIR, an absolute path on a
# local disk outside the repository, in one layout:
#   fixtures/<kind>-<name>/  inputs built once and then reused (fixtures/adoption-<recipe>,
#                            fixtures/bench-testdata)
#   work/<gate>/             a gate's scratch, reused by its next run
#   runs/<session>/          a gates/run session: session.json and one folder per gate
#   runs/<gate>-<name>/      a gate run on its own; every run directory is new, never reused
# A directory passed explicitly (--fixtures, --testdata, --out, ADOPTION_WORK_DIR) wins over the
# root. With neither, a gate stops with one line naming FERROFIN_GATES_DIR: there is no default.

# This library's own directory, taken when it is sourced: a caller may change directory later.
GATES_LIB_DIR=$(cd -P -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

# gates_require_local <dir>: refuse a directory on a network filesystem, judged by its nearest
# existing parent so nothing is created before the check. Every gate copies, boots and scans its
# fixtures repeatedly; that load belongs on a local disk, never a NAS. findmnt names the real type
# (fuse.sshfs, not just fuse), so local FUSE disks such as ntfs-3g or mergerfs still pass.
gates_require_local() {
  local dir=$1 types fs
  until [ -e "$dir" ]; do dir=$(dirname "$dir"); done
  types=$(findmnt -n -o FSTYPE -T "$dir") || { echo "refusing $1: cannot tell its filesystem" >&2; return 1; }
  for fs in $types; do
    case $fs in
      nfs*|cifs|smb*|9p|ceph|afs|glusterfs|davfs|fuse.sshfs|fuse.rclone|fuse.glusterfs|fuse.s3fs|fuse.davfs2)
        echo "refusing $1: it is on a network filesystem ($fs); use a local disk" >&2; return 1;;
    esac
  done
}

# The cores the measuring gates (bench, parity) run on by default, and how idle they must be first.
# shellcheck disable=SC2034  # read by the scripts that source this library
GATES_SERVER_CPUS=8-15 GATES_CLIENT_CPUS=16-19 GATES_CORE_IDLE_MIN=0.90

# gates_idle_check <server cpus> <client cpus> <minimum idle fraction>: whether those cores, their SMT
# siblings included (gates/bench/mem_sample.py), are idle enough to measure on. Sets GATES_TOPOLOGY and
# GATES_IDLE for the caller's record; a refusal is one line on stderr, and status 1.
gates_idle_check() {
  local sample=$GATES_LIB_DIR/bench/mem_sample.py
  GATES_TOPOLOGY=$(python3 "$sample" --topology "$1" "$2") || { echo "invalid CPU allocation: $1 and $2" >&2; return 1; }
  GATES_IDLE=$(python3 "$sample" --check "$(jq -r '.checked_cpus|map(tostring)|join(",")' <<<"$GATES_TOPOLOGY")") || return 1
  awk -v i="$GATES_IDLE" -v m="$3" -v c="$1,$2" 'BEGIN { if (i < m) {
    printf "cores %s are only %.0f%% idle (need %.0f%%): something else is using them\n", c, i*100, m*100; exit 1 } }' >&2
}

# gates_root <who>: the output root, absolute and physical (symlinks resolved); nothing is created.
# A refusal is one line on stderr starting with <who>, and status 2. Inside the repository, or the
# main checkout a worktree belongs to, it would reach `docker build .` and `git status`, so it is
# refused there.
gates_root() {
  local root=${FERROFIN_GATES_DIR:-} dir rest='' top common t
  [ -n "$root" ] || { echo "$1: set FERROFIN_GATES_DIR to the gates' output root, or pass the directory explicitly" >&2; return 2; }
  case $root in /*) ;; *) echo "$1: FERROFIN_GATES_DIR must be an absolute path, not $root" >&2; return 2;; esac
  root=${root%"${root##*[!/]}"}   # no trailing slash
  [ -n "$root" ] || { echo "$1: FERROFIN_GATES_DIR must not be /" >&2; return 2; }
  case $root/ in *//*|*/./*|*/../*) echo "$1: FERROFIN_GATES_DIR must be a plain path, without //, . or .. ($root)" >&2; return 2;; esac
  # The nearest existing ancestor resolved, the rest appended (realpath -m is not in busybox).
  dir=$root
  while [ ! -d "$dir" ]; do
    [ ! -e "$dir" ] || { echo "$1: FERROFIN_GATES_DIR ($root) is not a directory: $dir is a file" >&2; return 2; }
    [ ! -L "$dir" ] || { echo "$1: FERROFIN_GATES_DIR ($root) goes through $dir, a symlink to nothing" >&2; return 2; }
    rest=/${dir##*/}$rest; dir=${dir%/*}; dir=${dir:-/}
  done
  dir=$(cd -P -- "$dir" && pwd) || return 2
  root=${dir%/}$rest
  [ -n "$root" ] || { echo "$1: FERROFIN_GATES_DIR ($FERROFIN_GATES_DIR) resolves to /" >&2; return 2; }
  [ -n "${GATES_LIB_DIR:-}" ] || { echo "$1: cannot tell where the repository is" >&2; return 2; }
  top=${GATES_LIB_DIR%/*}
  # A worktree's main checkout too: its `docker build .` and `git status` reach the worktrees inside it.
  common=$(git -C "$top" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || common=''
  for t in "$top" "$( [ -n "$common" ] && cd -P -- "$common/.." 2>/dev/null && pwd)"; do
    [ -n "$t" ] || continue
    case $root/ in "$t"/*) echo "$1: FERROFIN_GATES_DIR ($root) is inside the repository; choose a directory outside it" >&2; return 2;; esac
  done
  gates_require_local "$root" || return 2
  printf '%s\n' "$root"
}

# gates_dir <who> <path>: <path> under the output root (not created), or status 2 as gates_root.
gates_dir() {
  local root
  root=$(gates_root "$1") || return 2
  printf '%s/%s\n' "$root" "$2"
}

# Stopping a gate. Bash 5.2 drops a trapped signal that arrives while it parses a $( … ): it prints
# "trap: line 2: unexpected EOF while looking for matching `)'" and carries on, and the gates parse them all
# the time, so a Ctrl-C or a TERM could go unanswered until every deadline had run out (gl-968; bash 5.3 is
# not affected, and a trap that only sets a flag fails the same way). gates/adoption/run.sh is not
# covered here: gl-920 is its fix.
#
# gates_supervise <dir> <script> <args…>: the first thing a gate script does. It runs the script again
# as a worker, from <dir> (where the caller started it, so relative arguments still resolve), in a session
# of its own, and only waits for it, so a signal always finds this shell in `wait`.
# A signal goes on at once, from the trap itself, to the worker's process group as a TERM (an
# asynchronous worker ignores INT and QUIT); then, a second later and every second after, to the worker
# alone, until its teardown has run gates_ack or it has ended: the worker keeps its own traps and
# teardown, and a TERM it dropped is simply sent again. This shell exits with 128 + the signal it got, or
# with the worker's status. In the worker it returns at once.
gates_supervise() {
  if [ -n "${GATES_WORKER:-}" ]; then unset GATES_WORKER; return 0; fi
  command -v setsid >/dev/null || { echo "$2: setsid (util-linux) is required" >&2; exit 2; }
  local w='' status=0 sig=0 waited own=''
  if [ -z "${GATES_ACK_DIR:-}" ]; then   # one per outermost gate; a gate it starts uses the same
    GATES_ACK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/gates-ack.XXXXXX") || exit 2
    export GATES_ACK_DIR; own=1
  fi
  trap 'sig=130; [ -z "$w" ] || kill -TERM -- "-$w" 2>/dev/null || true' INT
  trap 'sig=143; [ -z "$w" ] || kill -TERM -- "-$w" 2>/dev/null || true' TERM
  trap 'sig=129; [ -z "$w" ] || kill -TERM -- "-$w" 2>/dev/null || true' HUP
  trap 'sig=131; [ -z "$w" ] || kill -TERM -- "-$w" 2>/dev/null || true' QUIT
  # <&0: an asynchronous command's stdin would otherwise be /dev/null. Without job control the background
  # job leads no process group, so setsid makes the session in place and $! is its leader and group.
  (cd "$1" && GATES_WORKER=1 exec setsid "$BASH" "${@:2}") <&0 &
  w=$!
  # a signal that came before $w was set: best effort, as the group may not exist yet (the re-send covers it)
  [ "$sig" = 0 ] || kill -TERM -- "-$w" 2>/dev/null || true
  while :; do
    if [ "$sig" = 0 ] || [ -e "$GATES_ACK_DIR/$w" ]; then status=0; wait "$w" || status=$?; waited=1
    else
      waited=0; sleep 1 || true
      # bash 5.2 may have dropped it. The ack is looked for just before: a teardown that acks in the
      # microseconds between and gives TERM its default action back is the one case left.
      [ -e "$GATES_ACK_DIR/$w" ] || kill -TERM "$w" 2>/dev/null || true
    fi
    kill -0 "$w" 2>/dev/null || break
  done
  [ "$waited" = 1 ] || { status=0; wait "$w" || status=$?; }
  rm -f -- "$GATES_ACK_DIR/$w"   # a later process may be given its PID
  [ -z "$own" ] || { rm -f -- "$GATES_ACK_DIR"/*; rmdir -- "$GATES_ACK_DIR"; } 2>/dev/null || true
  [ "$sig" = 0 ] || exit "$sig"
  exit "$status"
}

# gates_ack: the first thing a gate's teardown does, once its signal handler has run: the TERM that
# started it is not sent again. Harmless on an ordinary exit.
gates_ack() { [ -z "${GATES_ACK_DIR:-}" ] || { : > "$GATES_ACK_DIR/$BASHPID"; } 2>/dev/null || true; }

# gates_stop <pid>: wait for a background child shell of the worker that keeps its own traps. It had
# the TERM gates_supervise sent the whole worker group; if it dropped it, it is sent again every second
# until its teardown has run gates_ack or it has ended. Never at once, and never once it has acked: its
# teardown may have given TERM back its default action, and a TERM then would cut it short.
gates_stop() {
  local ack=${GATES_ACK_DIR:-}/$1
  while [ -n "${GATES_ACK_DIR:-}" ] && kill -0 "$1" 2>/dev/null; do
    sleep 1 || true
    [ -e "$ack" ] || kill -TERM "$1" 2>/dev/null || true
  done
  wait "$1" 2>/dev/null || true
  [ -z "${GATES_ACK_DIR:-}" ] || rm -f -- "$ack"
}
