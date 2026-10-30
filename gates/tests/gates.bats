#!/usr/bin/env bats
# Tests for what the gates share (gates/lib.sh): the output root, its layout and the local-disk
# check, and how each gate's script resolves its directories from them. Nothing here needs docker,
# fixtures or a server: a refusal or the first missing input shows which directory was chosen.

bats_require_minimum_version 1.5.0

setup() {
  # gates/ copied into a repository of its own: gates/lib.sh refuses an output root inside the repository
  # it runs from, so a root under the test's directory would be refused when TMPDIR itself is inside this
  # checkout (the pre-push CI mirror keeps scratch files there). From the copy it is beside the repository.
  mkdir -p "$BATS_TEST_TMPDIR/repo" && cp -r "$BATS_TEST_DIRNAME/.." "$BATS_TEST_TMPDIR/repo/gates"
  git -C "$BATS_TEST_TMPDIR/repo" init -q
  GATES="$BATS_TEST_TMPDIR/repo/gates"
  # shellcheck source=gates/lib.sh
  . "$GATES/lib.sh"
  TMP="$BATS_TEST_TMPDIR/work"
  mkdir -p "$TMP/bin"
  cd "$TMP" || exit 1
  unset FERROFIN_GATES_DIR FERROFIN_ADOPTION_FIXTURES ADOPTION_WORK_DIR
  ROOT="$BATS_TEST_TMPDIR/root"
}

fake_findmnt() { # fake_findmnt <fstype[,fstype…]> — every path reports these mount types, one per line
  printf '#!/usr/bin/env bash\nprintf "%%s\\n" %s\n' "${1//,/ }" > bin/findmnt; chmod +x bin/findmnt
}

fake_tools() { # docker and taskset that do nothing, first on PATH, so a script gets as far as its inputs
  printf '#!/usr/bin/env bash\nexit 0\n' > bin/docker; printf '#!/usr/bin/env bash\nexit 0\n' > bin/taskset
  chmod +x bin/docker bin/taskset
  PATH="$TMP/bin:$PATH"
}

@test "gates_require_local: network filesystems are refused, local ones (FUSE included) accepted" {
  for fs in nfs nfs4 cifs smb3 9p ceph afs glusterfs fuse.sshfs fuse.rclone autofs,nfs; do
    fake_findmnt "$fs"
    run bash -c '. "$1/lib.sh"; PATH="$2:$PATH" gates_require_local "$3"' _ "$GATES" "$TMP/bin" "$TMP"
    [ "$status" -eq 1 ]
    [[ "$output" == *"network filesystem (${fs##*,})"* ]]
  done
  for fs in ext4 btrfs tmpfs fuseblk fuse.mergerfs; do
    fake_findmnt "$fs"
    run bash -c '. "$1/lib.sh"; PATH="$2:$PATH" gates_require_local "$3"' _ "$GATES" "$TMP/bin" "$TMP"
    [ "$status" -eq 0 ]
  done
  # a directory not created yet is judged by its nearest existing parent
  run gates_require_local "$TMP/not/yet/made"
  [ "$status" -eq 0 ]
  # a filesystem findmnt cannot name is refused, never assumed local
  printf '#!/usr/bin/env bash\nexit 1\n' > bin/findmnt
  run bash -c '. "$1/lib.sh"; PATH="$2:$PATH" gates_require_local "$3"' _ "$GATES" "$TMP/bin" "$TMP"
  [ "$status" -eq 1 ]
  [[ "$output" == *"cannot tell its filesystem"* ]]
}

@test "gates_root: unset, relative, /, not plain, a file, inside the repository (or through a symlink into it) or on a network share is refused; nothing is created" {
  run --separate-stderr gates_root who
  [ "$status" -eq 2 ]
  # shellcheck disable=SC2154  # $stderr is set by bats' run --separate-stderr
  [ "$stderr" = "who: set FERROFIN_GATES_DIR to the gates' output root, or pass the directory explicitly" ]
  FERROFIN_GATES_DIR=relative/root run --separate-stderr gates_root who
  [ "$status" -eq 2 ]
  [[ "$stderr" == *"must be an absolute path"* ]]
  for bad in "$ROOT/../x" "$ROOT/./x" "//$ROOT" "$ROOT//x"; do
    FERROFIN_GATES_DIR=$bad run --separate-stderr gates_root who
    [ "$status" -eq 2 ]
    [[ "$stderr" == *"must be a plain path, without //, . or .."* ]]
  done
  for bad in / //; do
    FERROFIN_GATES_DIR=$bad run --separate-stderr gates_root who
    [ "$status" -eq 2 ]
    [[ "$stderr" == *"must not be /"* ]]
  done
  ln -s / to-root
  FERROFIN_GATES_DIR="$TMP/to-root" run --separate-stderr gates_root who
  [ "$status" -eq 2 ]
  [[ "$stderr" == *"resolves to /"* ]]
  ln -s "$TMP/nothing-yet" dangling
  FERROFIN_GATES_DIR="$TMP/dangling/x" run --separate-stderr gates_root who
  [ "$status" -eq 2 ]
  [[ "$stderr" == *"goes through $TMP/dangling, a symlink to nothing"* ]]
  touch a-file
  for bad in "$TMP/a-file" "$TMP/a-file/under"; do
    FERROFIN_GATES_DIR=$bad run --separate-stderr gates_root who
    [ "$status" -eq 2 ]
    [[ "$stderr" == *"is not a directory: $TMP/a-file is a file"* ]]
  done
  repo=$(cd "$GATES/.." && pwd)
  FERROFIN_GATES_DIR="$repo/target/gates" run --separate-stderr gates_root who
  [ "$status" -eq 2 ]
  [[ "$stderr" == *"is inside the repository"* ]]
  ln -s "$repo" into-repo
  FERROFIN_GATES_DIR="$TMP/into-repo/gates-out" run --separate-stderr gates_root who
  [ "$status" -eq 2 ]
  [[ "$stderr" == *"($repo/gates-out) is inside the repository"* ]]
  fake_findmnt nfs4
  FERROFIN_GATES_DIR=$ROOT run bash -c '. "$1/lib.sh"; PATH="$2:$PATH" gates_root who' _ "$GATES" "$TMP/bin"
  [ "$status" -eq 2 ]
  [[ "$output" == *"network filesystem (nfs4)"* ]]
  [ ! -e "$ROOT" ]
}

@test "gates_root: a root inside the main checkout of a worktree is refused, beside it accepted" {
  git init -q main && git -C main -c user.name=t -c user.email=t@t commit -q --allow-empty -m t
  git -C main worktree add -q "$TMP/main/wt" && mkdir -p main/wt/gates && cp "$GATES/lib.sh" main/wt/gates/
  # shellcheck disable=SC2016  # expanded by the inner bash
  FERROFIN_GATES_DIR="$TMP/main/gates-out" run --separate-stderr bash -c '. "$1"; gates_root who' _ main/wt/gates/lib.sh
  [ "$status" -eq 2 ]
  [[ "$stderr" == *"is inside the repository"* ]]
  # shellcheck disable=SC2016  # expanded by the inner bash
  FERROFIN_GATES_DIR="$TMP/beside" run bash -c '. "$1"; gates_root who' _ main/wt/gates/lib.sh
  [ "$status" -eq 0 ]
  [ "$output" = "$(cd -P "$TMP" && pwd)/beside" ]
}

@test "gates_root and gates_dir: an accepted root is printed physical, without a trailing slash; the layout under it" {
  FERROFIN_GATES_DIR="$ROOT//" run gates_root who
  [ "$status" -eq 0 ]
  [ "$output" = "$ROOT" ]
  ln -s "$BATS_TEST_TMPDIR" link
  FERROFIN_GATES_DIR="$TMP/link/root" run gates_root who
  [ "$output" = "$(cd -P "$BATS_TEST_TMPDIR" && pwd)/root" ]
  FERROFIN_GATES_DIR=$ROOT run gates_dir who runs
  [ "$output" = "$ROOT/runs" ]
  run gates_dir who runs
  [ "$status" -eq 2 ]
  [ ! -e "$ROOT" ]
}

@test "bench/run.sh: the test data comes from --testdata, else the root; the run directory needs --out or the root" {
  run "$GATES/bench/run.sh"
  [ "$status" -eq 2 ]
  [ "$output" = "bench: set FERROFIN_GATES_DIR to the gates' output root, or pass the directory explicitly" ]
  run "$GATES/bench/run.sh" --testdata td   # the run directory still needs the root or --out
  [ "$status" -eq 2 ]
  [[ "$output" == *"set FERROFIN_GATES_DIR"* ]]
  FERROFIN_GATES_DIR=$ROOT run "$GATES/bench/run.sh"
  [ "$status" -eq 1 ]
  [[ "$output" == *"no $ROOT/fixtures/bench-testdata/ids.json"* ]]
  FERROFIN_GATES_DIR=$ROOT run "$GATES/bench/run.sh" --testdata td
  [[ "$output" == *"no $TMP/td/ids.json"* ]]
  run "$GATES/bench/run.sh" --testdata td --out out
  [[ "$output" == *"no $TMP/td/ids.json"* ]]
  mkdir taken
  FERROFIN_GATES_DIR=$ROOT run "$GATES/bench/run.sh" --out taken   # --out wins over the root
  [[ "$output" == *"$TMP/taken exists — a run directory is never reused"* ]]
  [ ! -e "$ROOT" ]
}

@test "bench/testdata/build.sh and gate.py: the root, unless a directory is given; with neither, one line naming FERROFIN_GATES_DIR" {
  run "$GATES/bench/testdata/build.sh"
  [ "$status" -eq 2 ]
  [ "${#lines[@]}" -eq 1 ]
  [[ "$output" == "build: set FERROFIN_GATES_DIR"* ]]
  mkdir -p "$ROOT/fixtures/bench-testdata/config" td/config   # build.sh refuses an existing config, naming it
  FERROFIN_GATES_DIR=$ROOT run "$GATES/bench/testdata/build.sh"
  [[ "$output" == "$ROOT/fixtures/bench-testdata/config exists"* ]]
  FERROFIN_GATES_DIR=$ROOT run "$GATES/bench/testdata/build.sh" td
  [[ "$output" == "$TMP/td/config exists"* ]]
  rm -r "$ROOT"
  run python3 "$GATES/bench/gate.py" ab a b --dry-run
  [ "$status" -eq 2 ]
  [ "${#lines[@]}" -eq 1 ]
  [[ "$output" == "gate: set FERROFIN_GATES_DIR"* ]]
  FERROFIN_GATES_DIR=$ROOT run python3 "$GATES/bench/gate.py" ab a b --dry-run
  [ "$status" -eq 0 ]
  [[ "${lines[0]}" == "ab session $ROOT/runs/bench-gate-ab-"* ]]
  run python3 "$GATES/bench/gate.py" ab a b --out out --testdata td --dry-run
  [ "$status" -eq 0 ]
  [[ "${lines[0]}" == "ab session $TMP/out:" ]]
  FERROFIN_GATES_DIR=$ROOT run python3 "$GATES/bench/gate.py" ab a b --out out --testdata td --dry-run   # the flags win
  [[ "${lines[0]}" == "ab session $TMP/out:" ]]
  [ ! -e "$ROOT" ]
}

@test "parity/run.sh: the corpus comes from --testdata, else the root; the flags win; with neither, one line naming FERROFIN_GATES_DIR" {
  fake_tools
  run "$GATES/parity/run.sh" --oracle o --ferrofin f
  [ "$status" -eq 2 ]
  [ "$output" = "parity: set FERROFIN_GATES_DIR to the gates' output root, or pass the directory explicitly" ]
  FERROFIN_GATES_DIR=$ROOT run "$GATES/parity/run.sh" --oracle o --ferrofin f
  [ "$status" -eq 2 ]
  [[ "$output" == *"no $ROOT/fixtures/bench-testdata/ids.json"* ]]
  run "$GATES/parity/run.sh" --oracle o --ferrofin f --testdata td --out out
  [[ "$output" == *"no $TMP/td/ids.json"* ]]
  FERROFIN_GATES_DIR=$ROOT run "$GATES/parity/run.sh" --oracle o --ferrofin f --testdata td --out out   # the flags win
  [[ "$output" == *"no $TMP/td/ids.json"* ]]
  mkdir taken
  FERROFIN_GATES_DIR=$ROOT run "$GATES/parity/run.sh" --oracle o --ferrofin f --out taken
  [[ "$output" == *"$TMP/taken exists: a run directory is never reused"* ]]
  [ ! -e "$ROOT" ]
}

@test "verify/run.sh: the run directory comes from --out, else the root; a missing image is refused before anything starts" {
  fake_tools
  run "$GATES/verify/run.sh" --image any
  [ "$status" -eq 2 ]
  [ "$output" = "verify: set FERROFIN_GATES_DIR to the gates' output root, or pass the directory explicitly" ]
  printf '#!/usr/bin/env bash\nexit 1\n' > bin/docker   # no image
  FERROFIN_GATES_DIR=$ROOT run "$GATES/verify/run.sh" --image any
  [ "$status" -eq 2 ]
  [ "$output" = "verify: image any missing" ]
  for bad in 0 65536 abc; do
    VERIFY_PORT=$bad run "$GATES/verify/run.sh" --image any --out out
    [ "$status" -eq 2 ]
    [[ "$output" == "verify: VERIFY_PORT "* ]]
  done
  mkdir taken
  FERROFIN_GATES_DIR=$ROOT run "$GATES/verify/run.sh" --image any --out taken
  [ "$output" = "verify: $TMP/taken exists: a run directory is never reused" ]
  [ ! -e "$ROOT" ]
}

# looper: a gate-shaped script under gates_supervise, in $TMP/looper.sh: a teardown on its EXIT trap that
# records the status it ended with, and a loop that parses a $( … ) as fast as it can, the shape that loses
# a trapped TERM under bash 5.2 (gl-968). It writes its worker's PID to out/worker once it loops. The loop
# never ends by itself: a dropped signal can break the parse of the $( … ) it landed in too, and a loop that
# tested the result would then end with its own status instead of the TERM's. It runs without set -e: with
# it, bash 5.2 would not drop the signal but exit at once with status 2, and the teardown would see 2 (bencher).
looper() {
  mkdir -p "$TMP/out"
  cat > "$TMP/looper.sh" <<'SH'
#!/usr/bin/env bash
set -uo pipefail
. "$GATES/lib.sh"
gates_supervise "$PWD" "$0" "$@"
OUT=$1
cleanup() { local rc=$?; trap '' INT TERM HUP; gates_ack; echo "$rc" > "$OUT/teardown"; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
echo "$BASHPID" > "$OUT/worker"
while :; do x=$(echo true) || :; done
SH
}

# session_pids <sid>: the processes still in that session (from /proc: busybox has no pgrep -s).
session_pids() {
  local stat pid s
  for stat in /proc/[0-9]*/stat; do
    read -r pid _ _ _ _ s _ 2>/dev/null < "$stat" || continue
    [ "$s" != "$1" ] || echo "$pid"
  done
}

@test "gates_supervise: a TERM to the group or the shell alone, a HUP or a Ctrl-C stops a worker looping on \$( … ), after its teardown" {
  looper
  export GATES
  mkdir acks
  set -m   # a background job keeps SIGINT (a non-interactive shell would ignore it), and leads its own group
  for how in group pid hup int group pid hup int group pid hup int; do   # under bash 5.2 some land mid-parse
    rm -f out/worker out/teardown
    TMPDIR=$TMP/acks bash looper.sh "$TMP/out" & leader=$!
    for _ in $(seq 1 100); do [ ! -s out/worker ] || break; sleep 0.05; done
    worker=$(cat out/worker)
    sleep "0.$((RANDOM % 3))"
    case $how in
      group) kill -TERM -- "-$leader"; want=143;;
      pid) kill -TERM "$leader"; want=143;;
      hup) kill -HUP "$leader"; want=129;;
      int) kill -INT "$leader"; want=130;;
    esac
    for _ in $(seq 1 40); do kill -0 "$leader" 2>/dev/null || break; sleep 0.1; done
    if kill -0 "$leader" 2>/dev/null; then
      kill -KILL -- "-$leader" "$worker" 2>/dev/null; set +m
      echo "$how: still running 4 s after the signal" >&2; return 1
    fi
    status=0; wait "$leader" || status=$?
    [ "$status" -eq "$want" ] || { set +m; echo "$how: exit $status, not $want" >&2; return 1; }
    [ "$(cat out/teardown)" = 143 ] || { set +m; echo "$how: the teardown saw $(cat out/teardown)" >&2; return 1; }
    [ -z "$(session_pids "$worker")" ] || { set +m; echo "$how: the worker's session outlived it" >&2; return 1; }
    [ -z "$(ls -A acks)" ] || { set +m; echo "$how: the supervisor left $(ls acks)" >&2; return 1; }
  done
  set +m
}

# bencher: bench/run.sh's shape under gates_supervise, in $TMP/bencher.sh <out> <bench|drop>: the worker's top
# level traps a signal and relays it (interrupt_run → gates_stop) to a background shell that loops on $( … ) and
# keeps its own traps, whose teardown gives TERM its default action back and takes a moment (a TERM sent to it
# again then would kill it before it wrote out/teardown). Bash 5.2 handles a TERM whose trap runs mid-parse in
# one of two ways (gl-989), and the second argument picks the server for each:
# - bench: bench's run_server, under set -e. There the trap's parse error exits the shell at once with status 2,
#   its EXIT trap included, so the teardown sees 2 instead of 143.
# - drop: a shell without set -e, which bash 5.2 lets drop the TERM. This one drops its first TERM every time,
#   so gates_stop has to send it again.
bencher() {
  mkdir -p "$TMP/out"
  cat > "$TMP/bencher.sh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
. "$GATES/lib.sh"
gates_supervise "$PWD" "$0" "$@"
OUT=$1
run_server() {
  cleanup() { local rc=$?; set +e; gates_ack; trap - EXIT INT TERM; sleep 0.5; echo "$rc" > "$OUT/teardown"; }
  trap cleanup EXIT
  if [ "$1" = drop ]; then set +e; trap 'trap "exit 143" TERM' TERM; else trap 'exit 143' TERM; fi
  echo "$BASHPID" > "$OUT/server"
  while :; do x=$(echo true) || :; done   # never ends by itself (looper)
}
SERVER_PID=
interrupt_run() { gates_ack; trap - INT TERM; [ -z "$SERVER_PID" ] || gates_stop "$SERVER_PID"; exit 130; }
trap interrupt_run INT TERM
echo "$BASHPID" > "$OUT/worker"
run_server "$2" &
SERVER_PID=$!
wait "$SERVER_PID"
SH
}

@test "gates_stop: bench's relay reaches a background shell looping on \$( … ), and its slow teardown is never cut short" {
  bencher
  export GATES
  # group and pid, each with bench's server (a TERM can land mid-parse) and with one that drops its first TERM
  for how in group:bench pid:bench group:drop pid:drop group:bench pid:bench; do
    rm -f out/worker out/server out/teardown
    LC_ALL=C setsid bash bencher.sh "$TMP/out" "${how#*:}" 2> out/err & leader=$!   # LC_ALL: bash's own messages
    for _ in $(seq 1 100); do [ ! -s out/server ] || break; sleep 0.05; done
    worker=$(cat out/worker)
    sleep "0.$((RANDOM % 3))"
    if [ "${how%:*}" = group ]; then kill -TERM -- "-$leader"; else kill -TERM "$leader"; fi
    for _ in $(seq 1 40); do kill -0 "$leader" 2>/dev/null || break; sleep 0.1; done
    if kill -0 "$leader" 2>/dev/null; then
      kill -KILL -- "-$leader" "-$worker" 2>/dev/null; echo "$how: still running 4 s after the TERM" >&2; cat out/err >&2
      return 1
    fi
    status=0; wait "$leader" || status=$?
    [ "$status" -eq 143 ] || { echo "$how: exit $status" >&2; cat out/err >&2; return 1; }
    # The teardown writes the status it saw only after its slow part: a TERM sent again then would leave nothing.
    [ -s out/teardown ] || { echo "$how: the server's teardown did not finish" >&2; cat out/err >&2; return 1; }
    # 143 is the TERM's own trap. Under set -e, bash 5.2 exits 2 instead when that trap fails to parse, and says so.
    teardown=$(cat out/teardown)
    [ "$teardown" = 143 ] || {
      [ "${how#*:}" = bench ] && [ "$teardown" = 2 ] &&
        grep -q "^bencher.sh: trap: line [0-9]*: unexpected EOF while looking for matching" out/err &&
        echo "# $how: the TERM's trap did not parse (bash 5.2); the server's teardown saw 2" >&3
    } || { echo "$how: the server's teardown saw $teardown" >&2; cat out/err >&2; return 1; }
    [ -z "$(session_pids "$worker")" ] || { echo "$how: the worker's session outlived it" >&2; cat out/err >&2; return 1; }
  done
}
