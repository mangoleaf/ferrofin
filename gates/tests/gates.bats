#!/usr/bin/env bats
# Tests for what the gates share (gates/lib.sh): the output root, its layout and the local-disk
# check, and how each gate's script resolves its directories from them. Nothing here needs docker,
# fixtures or a server: a refusal or the first missing input shows which directory was chosen.

bats_require_minimum_version 1.5.0

setup() {
  GATES="$(cd "$BATS_TEST_DIRNAME/.." && pwd)"
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
