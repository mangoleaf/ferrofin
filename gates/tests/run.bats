#!/usr/bin/env bats
# Tests for gates/run, the one entry point: its preflight and its session folder. They run a copy of
# gates/ in a throwaway repository, every gate's script replaced by a fake that records how it was
# called, against a fake docker; nothing is booted.

bats_require_minimum_version 1.5.0

setup() {
  GATES="$(cd "$BATS_TEST_DIRNAME/.." && pwd)"
  TMP="$BATS_TEST_TMPDIR"
  cd "$TMP" || exit 1
  mkdir -p repo bin
  cp -r "$GATES" repo/gates
  for gate in verify adoption parity bench; do fake_gate "repo/gates/$gate/run.sh"; done
  cat > repo/gates/bench/gate.py <<'EOS'
import os, sys
with open(os.environ["FAKE_CALLS"], "a") as calls:
    calls.write("bench-gate " + " ".join(sys.argv[1:]) + "\n")
sys.exit(int(os.environ.get("FAKE_RC_bench", "0")))
EOS
  cat > repo/gates/bench/mem_sample.py <<'EOS'
import os, sys
print('{"checked_cpus": [0]}' if sys.argv[1] == "--topology" else os.environ.get("FAKE_IDLE", "1.0"))
EOS
  # The real oracle check (gates/bench/oracle.py) on a pin of the copy's own, with a git whose ls-remote
  # lists $FAKE_LATEST_TAG as the latest Jellyfin release instead of asking the network.
  mkdir -p repo/crates/ferrofin-api/src/handlers
  echo 'pub const UPSTREAM_TAG: &str = "v12.2";' > repo/crates/ferrofin-api/src/handlers/mod.rs
  cat > bin/git <<EOS
#!/usr/bin/env bash
if [ "\$1" = ls-remote ]; then
  [ -n "\${FAKE_LATEST_TAG:-}" ] || exit 128
  printf 'abc\trefs/tags/%s\n' "\$FAKE_LATEST_TAG"; exit 0
fi
exec $(command -v git) "\$@"
EOS
  git -C repo init -q -b main && git -C repo add -A && git -C repo -c user.name=t -c user.email=t@t commit -q -m t
  COMMIT=$(git -C repo rev-parse HEAD)
  cat > bin/docker <<'EOS'
#!/usr/bin/env bash
[ "$1" != info ] || exit "${FAKE_DOCKER_DOWN:-0}"
case "$1 $2" in
  "image inspect")
    image=${!#}
    [[ " ${FAKE_IMAGES:-} " == *" $image "* ]] || exit 1
    case "$*" in
      *Labels*) echo "${FAKE_REVISION:-<no value>}";;
      *) if [ -e "$FAKE_CALLS.moved" ]; then echo sha256:moved; else echo "sha256:${image//[^a-z0-9]/}"; fi;;
    esac
    exit 0;;
  "ps -aq") case "$*" in *"name=^${FAKE_LEFT:-none}"*) echo deadbeef;; esac; exit 0;;
esac
echo "unexpected docker $*" >&2; exit 99
EOS
  for tool in k6 ffmpeg ffprobe taskset sqlite3; do printf '#!/usr/bin/env bash\nexit 0\n' > "bin/$tool"; done
  chmod +x bin/*
  PATH="$TMP/bin:$PATH"
  ORACLE=$(cat repo/gates/bench/testdata/jellyfin-image.txt)
  export FAKE_CALLS="$TMP/calls" FAKE_IMAGES="ferrofin:test ferrofin:old $ORACLE" FAKE_REVISION=$COMMIT
  export FAKE_LATEST_TAG=v12.2   # the pin is the latest Jellyfin release unless a test says otherwise
  export FERROFIN_GATES_DIR="$TMP/root"
  ROOT=$FERROFIN_GATES_DIR
  # the inputs the preflight looks for
  fixtures=$(bash -c '. "$1"; adoption_fixtures_dir test' _ repo/gates/adoption/lib.sh)
  mkdir -p "$fixtures/oracle" "$fixtures/media" "$ROOT/fixtures/bench-testdata/jellyfin"
  touch "$fixtures/oracle/smoke-jellyfin-12.4.txt" "$ROOT/fixtures/bench-testdata/ids.json" \
    "$ROOT/fixtures/bench-testdata/jellyfin/preparation.json"
  unset FAKE_RC_verify FAKE_RC_adoption FAKE_RC_parity FAKE_RC_bench FAKE_SLEEP FAKE_IDLE FAKE_LEFT FAKE_DOCKER_DOWN FAKE_MOVE \
    FERROFIN_ADOPTION_FIXTURES ADOPTION_ORACLE ADOPTION_WORK_DIR FERROFIN_BENCH_IMAGE PARITY_PIN   # a developer's own exports
}

fake_gate() { # fake_gate <path>: records "<gate> <arguments> work=… bench_image=…", makes --out, exits FAKE_RC_<gate>
  cat > "$1" <<'EOS'
#!/usr/bin/env bash
gate=$(basename "$(dirname "$0")")
printf '%s %s work=%s bench_image=%s\n' "$gate" "$*" "${ADOPTION_WORK_DIR:-}" "${FERROFIN_BENCH_IMAGE:-}" >> "$FAKE_CALLS"
echo $$ >> "$FAKE_CALLS.pids"
echo "the fake $gate ran"
while [ $# -gt 0 ]; do [ "$1" != --out ] || mkdir -p "$2"; shift; done
[ -z "${FAKE_SLEEP:-}" ] || sleep "$FAKE_SLEEP"
[ -z "${FAKE_MOVE:-}" ] || touch "$FAKE_CALLS.moved"   # someone retags the image under the session
rc=FAKE_RC_$gate; exit "${!rc:-0}"
EOS
  chmod +x "$1"
}

session() { # the one session directory made under the root
  local dirs=("$ROOT"/runs/*/)
  [ "${#dirs[@]}" -eq 1 ] && [ -d "${dirs[0]}" ] && echo "${dirs[0]%/}"
}

@test "gates/run: no output root is one line naming FERROFIN_GATES_DIR; --out is the root" {
  unset FERROFIN_GATES_DIR
  run repo/gates/run verify --image ferrofin:test
  [ "$status" -eq 2 ]
  [ "$output" = "run: set FERROFIN_GATES_DIR to the gates' output root, or pass the directory explicitly" ]
  run repo/gates/run verify --image ferrofin:test --out root --dry-run
  [ "$status" -eq 0 ]
  [[ "$output" == *"verify: gates/verify/run.sh --image ferrofin:test --out \$FERROFIN_GATES_DIR/runs/<session>/verify"* ]]
}

@test "gates/run --dry-run: the preflight and every gate's command, in order, and nothing started or created" {
  rm -rf "$ROOT"/runs
  run repo/gates/run all --image ferrofin:test --dry-run
  [ "$status" -eq 0 ]
  [[ "${lines[0]}" == "preflight: passed; image ferrofin:test"* ]]
  [[ "${lines[1]}" == "verify: gates/verify/run.sh --image ferrofin:test "* ]]
  [[ "${lines[2]}" == "adoption: env ADOPTION_WORK_DIR="*" gates/adoption/run.sh --fixtures $fixtures --image ferrofin:test" ]]
  [[ "${lines[3]}" == "parity: gates/parity/run.sh --oracle "*" --ferrofin ferrofin:test "*"--requests gates/parity/requests/sweep --requests gates/parity/requests/edge" ]]
  [[ "${lines[4]}" == "bench: env FERROFIN_BENCH_IMAGE=ferrofin:test gates/bench/run.sh "* ]]
  run repo/gates/run bench --image ferrofin:test --previous ferrofin:old --dry-run
  [[ "${lines[1]}" == "bench: python3 gates/bench/gate.py ab ferrofin:old ferrofin:test "* ]]
  [ ! -e "$FAKE_CALLS" ]
  [ ! -e "$ROOT/runs" ]
  [ ! -e "$ROOT/gates.lock" ]
}

@test "gates/run: the default image is named for the commit, and must carry it in its revision label" {
  run repo/gates/run verify --dry-run
  [ "$status" -eq 2 ]
  [[ "$output" == *"image ferrofin:${COMMIT:0:12} is missing: docker build --build-arg SOURCE_COMMIT=$COMMIT -t ferrofin:${COMMIT:0:12} ."* ]]
  git -C repo tag v9.9.9
  FAKE_IMAGES=ferrofin:v9.9.9 run repo/gates/run verify --dry-run
  [ "$status" -eq 0 ]
  FAKE_REVISION=0123 run repo/gates/run verify --image ferrofin:test --dry-run
  [ "$status" -eq 2 ]
  [[ "$output" == *"image ferrofin:test was built from 0123, not the checked-out $COMMIT"* ]]
  FAKE_REVISION='' run repo/gates/run verify --image ferrofin:test --dry-run
  [[ "$output" == *"was built from an unrecorded commit"* ]]
}

@test "gates/run: the preflight names every cause at once and starts nothing" {
  mkdir sysbin   # the host's tools without k6 and ffmpeg
  for dir in /usr/local/bin /usr/bin /bin; do
    for tool in "$dir"/*; do
      name=${tool##*/}
      case $name in k6|ffmpeg) continue;; esac
      [ -e "sysbin/$name" ] || [ -L "sysbin/$name" ] || ln -s "$tool" "sysbin/$name"
    done
  done
  rm bin/k6 bin/ffmpeg
  rm "$ROOT/fixtures/bench-testdata/ids.json"
  ( exec 8>"$ROOT/gates.lock"; flock 8; exec sleep 30 ) 3>&- & holder=$!   # kill ends the holder itself
  for _ in $(seq 1 50); do flock -n "$ROOT/gates.lock" true || break; sleep 0.1; done
  PATH="$TMP/bin:$TMP/sysbin" FAKE_IDLE=0.5 FAKE_LEFT=parity- run repo/gates/run all --image ferrofin:missing
  kill "$holder"
  [ "$status" -eq 2 ]
  [[ "$output" == *"run: k6 is not installed (bench needs it)"* ]]
  [[ "$output" == *"run: ffmpeg is not installed (verify needs it)"* ]]
  [[ "$output" == *"run: image ferrofin:missing is missing"* ]]
  [[ "$output" == *"the benchmark corpus is not built in $ROOT/fixtures/bench-testdata"* ]]
  [[ "$output" == *"cores 8-15,16-19 are only 50% idle (need 90%)"* ]]
  [[ "$output" == *"a parity-* container is left from another run"* ]]
  [[ "$output" == *"another gates session is running under $ROOT"* ]]
  [[ "${lines[-1]}" == "run: refused before any gate started" ]]
  [ ! -e "$FAKE_CALLS" ]
  [ ! -e "$ROOT/runs" ]
}

@test "gates/run: a session folder per run, named for the branch and the minute, with session.json and a folder per gate" {
  git -C repo checkout -q -b feat/gates
  run repo/gates/run verify,adoption --image ferrofin:test
  [ "$status" -eq 0 ]
  s=$(session)
  [[ "$s" == "$ROOT/runs/feat-gates-"[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]-[0-9][0-9][0-9][0-9] ]]
  [ -d "$s/verify" ] && [ -d "$s/adoption" ] && [ ! -e "$s/parity" ]
  grep -qx "verify --image ferrofin:test --out $s/verify work= bench_image=" "$FAKE_CALLS"
  grep -qx "adoption --fixtures $fixtures --image ferrofin:test work=$s/adoption bench_image=" "$FAKE_CALLS"
  [ "$(jq -r '[.branch, .commit, .dirty, .image.ref, .oracle.image] | join(" ")' "$s/session.json")" = \
    "feat/gates $COMMIT false ferrofin:test $(cat repo/gates/bench/testdata/jellyfin-image.txt)" ]
  [ "$(jq -r '.gates | to_entries | map("\(.key)=\(.value.status)/\(.value.exit)") | join(" ")' "$s/session.json")" = \
    "verify=passed/0 adoption=passed/0" ]
  grep -qx "the fake verify ran" "$s/verify.log"
  [[ "$output" == *"report: $s/index.html"* ]]   # the session's report, with every gate in it
  grep -q "<th>verify</th>" "$s/index.html"
  grep -q "| bench | not requested | not run |" "$s/report.md"
}

@test "gates/run: a failed gate does not stop the next one, and the run exits 1" {
  FAKE_RC_verify=3 run repo/gates/run verify,bench --image ferrofin:test --previous ferrofin:old
  [ "$status" -eq 1 ]
  s=$(session)
  [ "$(jq -r '.gates | to_entries | map("\(.key)=\(.value.status)/\(.value.exit)") | join(" ")' "$s/session.json")" = \
    "verify=failed/3 bench=passed/0" ]
  grep -qx "bench-gate ab ferrofin:old ferrofin:test --out $s/bench" "$FAKE_CALLS"
  [ "$(jq -r '.previous.ref' "$s/session.json")" = ferrofin:old ]
}

@test "gates/run: a terminated run stops the running gate, records it as interrupted and the rest as skipped" {
  FAKE_SLEEP=30 repo/gates/run verify,adoption --image ferrofin:test > out.txt 2>&1 & pid=$!
  for _ in $(seq 1 100); do grep -q '^verify ' "$FAKE_CALLS" 2>/dev/null && break; sleep 0.1; done
  worker=$(pgrep -P "$pid")   # the run goes to a worker in a session of its own (gates_supervise)
  [ -n "$worker" ]
  kill -TERM "$pid"
  wait "$pid" && status=0 || status=$?
  [ "$status" -eq 143 ]
  [ -z "$(pgrep -s "$worker")" ]
  s=$(session)
  [ "$(jq -r '.gates | to_entries | map("\(.key)=\(.value.status)/\(.value.note // "")") | join(" ")' "$s/session.json")" = \
    "verify=failed/interrupted adoption=skipped/not reached" ]
  [ "$(jq -r '.ended != null' "$s/session.json")" = true ]
  grep -q "| verify | failed (exit 143), interrupted |" "$s/report.md"   # an interrupted session is reported too
  run ! kill -0 "$(cat "$FAKE_CALLS.pids")"   # the gate was stopped, not left running
  run ! grep -q '^adoption ' "$FAKE_CALLS"
}

@test "gates/run: a TERM to its process group stops the gate and leaves nothing of the run's own session" {
  FAKE_SLEEP=30 setsid repo/gates/run verify,adoption --image ferrofin:test > out.txt 2>&1 & pid=$!
  for _ in $(seq 1 100); do grep -q '^verify ' "$FAKE_CALLS" 2>/dev/null && break; sleep 0.1; done
  worker=$(pgrep -P "$pid")   # the run goes to a worker in a session of its own (gates_supervise)
  [ -n "$worker" ]
  kill -TERM -- "-$pid"
  wait "$pid" && status=0 || status=$?
  [ "$status" -eq 143 ]
  [ "$(jq -r '.gates.verify.note' "$(session)/session.json")" = interrupted ]
  run ! kill -0 "$(cat "$FAKE_CALLS.pids")"
  [ -z "$(pgrep -s "$worker")" ]
}

@test "gates/run: -h prints the usage from any directory; no gate is the usage and status 2" {
  cd repo/gates
  run ./run -h
  [ "$status" -eq 0 ]
  [[ "${lines[0]}" == "gates/run <verify|adoption|parity|bench|all>"* ]]
  run ./run
  [ "$status" -eq 2 ]
  [[ "${lines[0]}" == "gates/run <verify|adoption|parity|bench|all>"* ]]
  run bash -c 'cd "$1/verify" && ./run.sh -h' _ "$GATES"   # the real one: the copy's is a fake
  [ "$status" -eq 0 ]
  [[ "${lines[0]}" == "gates/verify/run.sh "* ]]
}

@test "gates/run: the release A/B gate runs against the branch's pin and records a newer Jellyfin release" {
  local note="measured against Jellyfin v12.2, the release this branch pins; v99.0 is out"
  FAKE_LATEST_TAG=v99.0 run repo/gates/run bench --image ferrofin:test --previous ferrofin:old --dry-run
  [ "$status" -eq 0 ]
  [[ "$output" == *"oracle: $note"* ]]
  [[ "$output" == *"preflight: passed"* ]]
  FAKE_LATEST_TAG=v99.0 run repo/gates/run bench --image ferrofin:test --previous ferrofin:old
  [ "$status" -eq 0 ]
  grep -q "^bench-gate ab ferrofin:old ferrofin:test " "$FAKE_CALLS"
  [ "$(jq -c .oracle "$(session)/session.json")" = "$(jq -nc --arg i "$ORACLE" --arg n "$note" '{image: $i, tag: "v12.2", note: $n}')" ]
  grep -qF -- "- **Jellyfin oracle:** v12.2 ($ORACLE) — $note" "$(session)/report.md"
  unset FAKE_LATEST_TAG   # a lookup that fails is noted too, and refuses nothing
  run repo/gates/run bench --image ferrofin:test --previous ferrofin:old --dry-run
  [ "$status" -eq 0 ]
  [[ "$output" == *"oracle: measured against Jellyfin v12.2, the release this branch pins; the latest Jellyfin release could not be looked up"* ]]
}

@test "gates/run: the release A/B gate's refusals stop the preflight: an unreadable pin, a missing previous image, --previous without bench" {
  echo 'pub const UPSTREAM_TAG: &str = "v12.3-rc1";' > repo/crates/ferrofin-api/src/handlers/mod.rs
  run repo/gates/run bench --image ferrofin:test --previous ferrofin:old --dry-run
  [ "$status" -eq 2 ]
  [[ "$output" == *"UPSTREAM_TAG v12.3-rc1"*"is not a Jellyfin release tag"* ]]
  [[ "$output" == *"run: the Jellyfin oracle pin cannot be read (see above)"* ]]
  git -C repo checkout -q -- crates
  run repo/gates/run bench --image ferrofin:test --previous ferrofin:gone --dry-run
  [[ "$output" == *"image ferrofin:gone (--previous) is missing"* ]]
  run repo/gates/run verify --image ferrofin:test --previous ferrofin:old --dry-run
  [[ "$output" == *"--previous is for the bench gate, which is not selected"* ]]
}

@test "gates/run: docker that does not answer, a missing oracle image or adoption fixtures not built are named" {
  FAKE_DOCKER_DOWN=1 run repo/gates/run verify --image ferrofin:test --dry-run
  [ "$status" -eq 2 ]
  [[ "$output" == *"docker does not answer"* ]]
  [[ "$output" != *"is missing"* ]]
  FAKE_IMAGES=ferrofin:test run repo/gates/run parity --image ferrofin:test --dry-run
  [[ "$output" == *"the Jellyfin oracle image is missing: docker pull $ORACLE"* ]]
  rm -r "${fixtures:?}/media"
  run repo/gates/run adoption --image ferrofin:test --dry-run
  [[ "$output" == *"the adoption fixtures are not built in $fixtures (no media)"* ]]
  ADOPTION_ORACLE=12.1 run repo/gates/run adoption --image ferrofin:test --dry-run
  [[ "$output" == *"(no oracle/smoke-jellyfin-12.1.txt)"* ]]
}

@test "gates/run: the adoption gate runs on the fixtures the preflight checked, whatever FERROFIN_ADOPTION_FIXTURES says" {
  FERROFIN_ADOPTION_FIXTURES=/elsewhere run repo/gates/run adoption --image ferrofin:test
  [ "$status" -eq 0 ]
  grep -q "^adoption --fixtures $fixtures " "$FAKE_CALLS"
}

@test "gates/run: a rerun in the same minute gets -run2; a detached HEAD names the session for its commit" {
  git -C repo checkout -q --detach
  for minute in "$(date +%Y%m%d-%H%M)" "$(date -d '+1 min' +%Y%m%d-%H%M)"; do
    mkdir -p "$ROOT/runs/detached-${COMMIT:0:12}-$minute"
  done
  run repo/gates/run verify --image ferrofin:test
  [ "$status" -eq 0 ]
  compgen -G "$ROOT/runs/detached-${COMMIT:0:12}-[0-9]*-[0-9]*-run2" >/dev/null
}

@test "gates/run: an image retagged during the session fails the gates still to come, which never start" {
  FAKE_MOVE=1 run repo/gates/run verify,adoption --image ferrofin:test
  [ "$status" -eq 1 ]
  s=$(session)
  [ "$(jq -r '.gates | to_entries | map("\(.key)=\(.value.status)") | join(" ")' "$s/session.json")" = "verify=passed adoption=failed" ]
  [[ "$(jq -r .gates.adoption.note "$s/session.json")" == "the image ferrofin:test moved off sha256:"* ]]
  run ! grep -q '^adoption ' "$FAKE_CALLS"
}
