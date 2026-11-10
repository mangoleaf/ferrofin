#!/usr/bin/env bats
# Tests for the round-trip harness (gates/roundtrip/lib.sh, run.sh and its stages). Nothing here needs
# docker, a Jellyfin image or a real fixture: tests/fake_docker plays each container with
# tests/fake_server.py, so whole routes run, and records every docker call.

bats_require_minimum_version 1.5.0   # run --separate-stderr

setup() {
  ROUNDTRIP="$BATS_TEST_DIRNAME/.."
  # shellcheck source=gates/roundtrip/lib.sh
  . "$ROUNDTRIP/lib.sh"
  TMP=$BATS_TEST_TMPDIR
  cd "$TMP" || exit 1   # the files a test writes stay in its own directory
  mkdir -p bin state fixtures/media
  ln -s "$BATS_TEST_DIRNAME/fake_docker" bin/docker
  export FAKE_STATE=$TMP/state PATH=$TMP/bin:$PATH
  for spec in jellyfin-10.11.8:10.11.8 jellyfin-12.4-from-123:12.2.0; do
    python3 "$BATS_TEST_DIRNAME/fake_server.py" copy "fixtures/${spec%%:*}"
    echo "${spec#*:}" > "fixtures/${spec%%:*}/fake-generation"
  done
  unset FERROFIN_ADOPTION_FIXTURES FERROFIN_GATES_DIR ADOPTION_USER ROUNDTRIP_READY_TIMEOUT FAKE_RUN_DELAY FAKE_EXIT_CODE \
    FAKE_JELLYFIN
}

teardown() { # no fake server outlives its test
  local pid
  for pid in "$TMP"/state/*.pid; do [ ! -f "$pid" ] || kill "$(cat "$pid")" 2>/dev/null || true; done
}

run_gate() { # run_gate <work> [args…]: run.sh on the fake fixtures and the fake docker
  local work=$1; shift
  "$ROUNDTRIP/run.sh" --image ferrofin:test --jellyfin-image jellyfin/jellyfin:test --fixtures "$TMP/fixtures" \
    --work "$work" "$@"
}

started() { grep '^run ' "$TMP/state/calls" | grep -o -- "--name ${1:-}[^ ]*" | sed 's/--name //' | sort; }
removed() { [ ! -f "$TMP/state/removed" ] || grep "^${1:-}" "$TMP/state/removed" | sort -u; }   # by name: the fake's log

# --- names, ids and images -------------------------------------------------------------------------

@test "names: containers and the image tag carry the run id" {
  [ "$(rt_name 20261110-120000-42 10.11.8 jellyfin)" = rt-20261110-120000-42-10.11.8-jellyfin ]
  [ "$(rt_image_tag 20261110-120000-42)" = ferrofin-roundtrip:20261110-120000-42 ]
}

@test "run id: a new one names this shell's PID; a bad one is refused" {
  [[ "$(rt_new_run_id)" =~ ^[0-9]{8}-[0-9]{6}-$$$ ]]
  rt_valid_run_id 20261110-120000-42
  rt_valid_run_id agent.a-1
  run ! rt_valid_run_id Upper
  run ! rt_valid_run_id 'a b'
  run ! rt_valid_run_id ../escape
  run ! rt_valid_run_id ''
}

@test "jellyfin image: the minor release of UPSTREAM_TAG" {
  echo 'pub const UPSTREAM_TAG: &str = "v12.4";' > handlers.rs
  [ "$(rt_jellyfin_image handlers.rs)" = jellyfin/jellyfin:12.4 ]
  echo 'pub const UPSTREAM_TAG: &str = "v12.5.1";' > handlers.rs
  [ "$(rt_jellyfin_image handlers.rs)" = jellyfin/jellyfin:12.5 ]
  echo 'nothing' > handlers.rs
  run --separate-stderr rt_jellyfin_image handlers.rs
  [ "$status" -eq 2 ]
  # shellcheck disable=SC2154  # $stderr is set by bats' run --separate-stderr
  [[ "$stderr" == *"cannot read UPSTREAM_TAG"* ]]
}

@test "jellyfin image: the repository's own pin is readable" {
  [[ "$(rt_jellyfin_image "$ROUNDTRIP/../../crates/ferrofin-api/src/handlers.rs")" =~ ^jellyfin/jellyfin:[0-9]+\.[0-9]+$ ]]
}

@test "start: the port is one docker picked, never a fixed one, and the container is the cidfile's" {
  port=$(rt_start rt-x-route-ferrofin "$TMP/x.cid" ferrofin:test -v "$TMP/fixtures/jellyfin-10.11.8:/config")
  [[ "$port" =~ ^[0-9]+$ ]]
  grep -q -- '-p 127.0.0.1::8096 ' state/calls
  grep -q -- 'HTTPS_PROXY=http://127.0.0.1:9' state/calls
  id=$(cat x.cid)
  [[ "$id" == cid-* ]]   # docker's id, not the name
  rt_remove "$TMP/x.cid"
  [ ! -e x.cid ]
  grep -qx "rm -f $id" state/calls
  [ "$(removed)" = rt-x-route-ferrofin ]
}

@test "start: a container name another run holds is not started, and nothing of that run is removed" {
  echo rt-x-route-ferrofin > state/rt-x-route-ferrofin.name   # another run's container
  run ! rt_start rt-x-route-ferrofin "$TMP/x.cid" ferrofin:test
  [ ! -s x.cid ]
  rt_remove "$TMP/x.cid"
  run ! grep -q '^rm ' state/calls
}

@test "reasons_within: nothing for success, the output or a name for a failure, a reason at the deadline" {
  [ -z "$(reasons_within 5 x true)" ]
  [ "$(reasons_within 5 x sh -c 'echo boom; exit 3')" = boom ]
  [ "$(reasons_within 5 x false)" = "x failed" ]
  [ "$(reasons_within 1 x sh -c 'echo partial; exec sleep 30')" = "partial; x had no result within 1 s" ]
}

@test "not up: a running server is a deadline, an exited one is named with its status" {
  echo rt-x > state/rt-x.name   # a container that has exited, with status 4
  echo 4 > state/rt-x.exit
  [ "$(rt_not_up rt-x 7)" = "exited with status 4 before it answered" ]
  sleep 30 & echo $! > state/rt-x.pid
  [ "$(rt_not_up rt-x 7)" = "never became ready within 7 s" ]
  kill "$(cat state/rt-x.pid)"
}

# --- the start-failed line ---------------------------------------------------------------------------

@test "start failed: Jellyfin's critical start line matches, an ordinary error does not" {
  grep -qE -- "$RT_JELLYFIN_START_FAILED" <<<'[16:44:10] [FTL] [7] Main: Error while starting server'
  run ! grep -qE -- "$RT_JELLYFIN_START_FAILED" <<<'[16:44:10] [ERR] [7] Main: Error while starting server'
  run ! grep -qE -- "$RT_JELLYFIN_START_FAILED" <<<'[16:44:10] [FTL] [7] InternalDatabaseMigration: Error: SQLite Error 1'
}

# --- comparing the answers ---------------------------------------------------------------------------

answers() { # answers <file> <played> [plugins]
  cat > "$1" <<EOS
200  12.4.0 Jellyfin Server                        /System/Info/Public
200  ${3:-AudioDB,TMDb}                            /Plugins
200  $2                                            /Items?UserId=dddddddddddddddddddddddddddddddd&Filters=IsPlayed&Limit=0
userdata synthetic-admin 0646d8a3c487e7c559c99be73da1bdde played=1 fav=0 resume=0
login roundtrip-user 200 token=yes
collection Playlist 2 Round trip playlist
EOS
}

@test "compare: equal answers say nothing, nor do the probes that differ by design" {
  answers oracle.txt 3 "AudioDB,Intro Skipper,LDAP-Auth"; answers jellyfin.txt 3
  run rt_compare_answers oracle.txt jellyfin.txt
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "compare: a lost watch state is a reason naming how many lines and the first" {
  answers oracle.txt 3; answers jellyfin.txt 2
  sed -i 's/played=1/played=0/' jellyfin.txt
  run rt_compare_answers oracle.txt jellyfin.txt
  [[ "$output" == "4 line(s) differ, first: < 200  3 "* ]]
  [[ "$output" == *"(diff oracle.txt jellyfin.txt)" ]]
}

@test "compare: user data on another item is a reason, though the normalised lines are equal" {
  answers oracle.txt 3; answers jellyfin.txt 3
  sed -i 's/0646d8a3c487e7c559c99be73da1bdde/1559654e1fb057800c6600ca97f14dbb/' jellyfin.txt
  diff -q <(adoption_normalise oracle.txt) <(adoption_normalise jellyfin.txt)   # equal once ids are redacted
  run rt_compare_answers oracle.txt jellyfin.txt
  [[ "$output" == "2 line(s) differ, first: < userdata synthetic-admin 0646d8a3c487e7c559c99be73da1bdde played=1"* ]]
}

@test "compare: a play count or a like on another item is a reason, though the normalised lines are equal" {
  answers oracle.txt 3; answers jellyfin.txt 3
  echo "userplays synthetic-admin 0646d8a3c487e7c559c99be73da1bdde count=2 likes=true" >> oracle.txt
  echo "userplays synthetic-admin 1559654e1fb057800c6600ca97f14dbb count=2 likes=true" >> jellyfin.txt
  diff -q <(adoption_normalise oracle.txt) <(adoption_normalise jellyfin.txt)   # equal once ids are redacted
  run rt_compare_answers oracle.txt jellyfin.txt
  [[ "$output" == "2 line(s) differ, first: < userplays synthetic-admin 0646d8a3c487e7c559c99be73da1bdde count=2"* ]]
}

@test "compare: no answers from Jellyfin is a reason" {
  answers oracle.txt 3; : > jellyfin.txt
  run rt_compare_answers oracle.txt jellyfin.txt
  [ "$output" = "no answers from Jellyfin" ]
}

# --- run.sh refusals: nothing starts --------------------------------------------------------------

@test "run.sh: an unknown route, a bad run id, a reused work directory, a held id and a missing image are refused" {
  run --separate-stderr run_gate "$TMP/w1" --only 9.9
  [ "$status" -eq 2 ]
  # shellcheck disable=SC2154  # $stderr is set by bats' run --separate-stderr
  [[ "$stderr" == *"no route 9.9 in routes.sh"* ]]
  run --separate-stderr run_gate "$TMP/w2" --run-id 'Bad Id'
  [ "$status" -eq 2 ]; [[ "$stderr" == *"--run-id must be"* ]]
  mkdir "$TMP/w3"
  run --separate-stderr run_gate "$TMP/w3"
  [ "$status" -eq 2 ]; [[ "$stderr" == *"is never reused"* ]]
  echo other > state/tag-held   # another run's tag: ferrofin-roundtrip:held
  run --separate-stderr run_gate "$TMP/w4" --run-id held
  [ "$status" -eq 2 ]; [[ "$stderr" == *"another run has the id held"* ]]
  [ -f state/tag-held ]
  run --separate-stderr "$ROUNDTRIP/run.sh" --image missing:x --jellyfin-image jellyfin/jellyfin:test \
    --fixtures "$TMP/fixtures" --work "$TMP/w5"
  [ "$status" -eq 2 ]; [[ "$stderr" == *"image missing:x missing"* ]]
  run ! grep -q '^run ' state/calls
  [ ! -e "$TMP/w4" ]
  [ ! -e "$TMP/w5" ]
}

# --- whole runs ------------------------------------------------------------------------------------

@test "run.sh: the 12.4 route passes, and the 10.11.8 route fails naming the migration Jellyfin could not apply" {
  run run_gate "$TMP/w" --run-id both
  [ "$status" -eq 1 ]
  [[ "$output" == *"PASS  12.4-from-123  "* ]]
  [[ "$output" == *"FAIL  10.11.8        its start failed; migration 20260113102337_AddLinkedChildrenTable failed: SQLite Error 1: 'table \"LinkedChildren\" already exists'; "* ]]
  # every check of the passing route ran and passed, Jellyfin's ones on its answers too
  checks=$(jq -r 'select(.event == "check" and .fixture == "12.4-from-123") | "\(.stage)|\(.check)|\(.verdict)"' "$TMP/w/results.jsonl")
  [ "$(grep -c '|PASS$' <<<"$checks")" -eq 11 ]
  grep -qx "Jellyfin|answers equal Ferrofin's before the stop|PASS" <<<"$checks"
  grep -q '^login roundtrip-user 200 token=yes$' "$TMP/w/12.4-from-123/answers.txt"
  cmp "$TMP/w/12.4-from-123/oracle.txt" "$TMP/w/12.4-from-123/answers.txt" >/dev/null ||
    diff <(adoption_normalise "$TMP/w/12.4-from-123/oracle.txt") <(adoption_normalise "$TMP/w/12.4-from-123/answers.txt")
  # the passing route keeps its logs and answers, not its copies; the failing one keeps its copies
  [ ! -e "$TMP/w/12.4-from-123/jellyfin" ]
  [ ! -e "$TMP/w/12.4-from-123/ferrofin" ]
  [ -d "$TMP/w/10.11.8/jellyfin" ]
  jq -e 'select(.event == "check" and .fixture == "10.11.8" and .stage == "Jellyfin" and .check == "ready")
         | .verdict == "FAIL"' "$TMP/w/results.jsonl" >/dev/null
  [ -f "$TMP/w/report.html" ]
  grep -q 'AddLinkedChildrenTable' "$TMP/w/report.html"
  [ "$(started rt-both-)" = "$(removed rt-both-)" ]
  grep -qx 'rmi ferrofin-roundtrip:both' state/calls
}

@test "run.sh: a Ferrofin that exits at once fails its route, Jellyfin is never booted, and the run removes what it made" {
  run "$ROUNDTRIP/run.sh" --image ferrofin:dead --jellyfin-image jellyfin/jellyfin:test --fixtures "$TMP/fixtures" \
    --work "$TMP/w" --run-id solo --only 10.11.8
  [ "$status" -eq 1 ]
  [[ "$output" == *"FAIL  10.11.8        did not start: docker published no port for rt-solo-10.11.8-ferrofin"* ]]
  # a server that never ran is not judged on its stop
  [ "$(jq -r 'select(.event == "check") | .check' "$TMP/w/results.jsonl")" = ready ]
  grep -qx 'tag sha256:fake-ferrofin-dead ferrofin-roundtrip:solo' state/calls
  [ "$(started)" = rt-solo-10.11.8-ferrofin ]
  [ "$(removed)" = rt-solo-10.11.8-ferrofin ]
  grep -qx 'rmi ferrofin-roundtrip:solo' state/calls
  [ -f "$TMP/w/report.html" ]
}

@test "run.sh: two runs at once overlap, share no container, tag or run directory, and each removes only its own" {
  # Their run directories are the defaults under the output root, which must lie outside the repository:
  # the gates go to a repository of their own here, as gates/adoption/tests/adoption.bats has them.
  mkdir -p repo/gates
  gates=$(cd "$ROUNDTRIP/.." && pwd)   # resolved: a source ending in .. would copy into repo/ itself
  cp -r "$gates/lib.sh" "$gates/adoption" "$gates/roundtrip" repo/gates/
  git -C repo init -q
  pids=()
  for side in a b; do
    FERROFIN_GATES_DIR=$TMP/gates FAKE_RUN_DELAY=1 repo/gates/roundtrip/run.sh --image ferrofin:test \
      --jellyfin-image jellyfin/jellyfin:test --fixtures "$TMP/fixtures" > "$side.out" 2>&1 &
    pids+=("$!")
  done
  for pid in "${pids[@]}"; do wait "$pid" || true; done
  ida=$(sed -n 's/^roundtrip: run \([^,]*\),.*/\1/p' a.out)
  idb=$(sed -n 's/^roundtrip: run \([^,]*\),.*/\1/p' b.out)
  [ -n "$ida" ]
  [ -n "$idb" ]
  [ "$ida" != "$idb" ]
  # they ran at the same time: both had started a container before either removed one
  first_rm=$(grep -n '^rm -f ' state/calls | head -n 1 | cut -d: -f1)
  for id in "$ida" "$idb"; do
    first_run=$(grep -n "^run .*--name rt-$id-" state/calls | head -n 1 | cut -d: -f1)
    [ "$first_run" -lt "$first_rm" ]
    # both routes, both servers where they got that far, under this run's names; exactly those removed
    [ "$(started "rt-$id-" | wc -l)" -eq 4 ]
    [ "$(started "rt-$id-")" = "$(removed "rt-$id-")" ]
    grep -qx "rmi ferrofin-roundtrip:$id" state/calls
  done
  [ "$(removed)" = "$(started)" ]   # nothing removed that no run started
  run ! grep -q -- '-p 127.0.0.1:[0-9]' state/calls
  grep -q '^PASS  12.4-from-123' a.out
  grep -q '^PASS  12.4-from-123' b.out
  [ -f "$TMP/gates/runs/roundtrip-$ida/report.html" ]
  [ -f "$TMP/gates/runs/roundtrip-$idb/report.html" ]
}

@test "run.sh: a Jellyfin that lost a write fails the comparison, naming the first difference" {
  FAKE_JELLYFIN=loses run run_gate "$TMP/w" --only 12.4-from-123
  [ "$status" -eq 1 ]
  [[ "$output" == *"FAIL  12.4-from-123  2 line(s) differ, first: < userdata synthetic-admin "*" fav=1 "* ]]
  jq -e 'select(.event == "check" and .check == "answers equal Ferrofin'"'"'s before the stop") | .verdict == "FAIL"' \
    "$TMP/w/results.jsonl" >/dev/null
}

@test "run.sh: a Ferrofin that does not exit 0 on TERM fails its stop, and Jellyfin is not booted on its copy" {
  FAKE_EXIT_CODE=137 run run_gate "$TMP/w" --only 12.4-from-123
  [ "$status" -eq 1 ]
  [[ "$output" == *"FAIL  12.4-from-123  exited with status 137 after TERM"* ]]
  [ "$(started)" = "$(started rt-)" ]
  run ! grep -q -- '--name rt-[^ ]*-jellyfin ' state/calls
}

@test "run.sh: a run stopped mid-stage removes the containers it started and its tag" {
  # run.sh itself in the background, not run_gate: $! must be the gate's shell, not a bats subshell
  FAKE_JELLYFIN=hang ROUNDTRIP_READY_TIMEOUT=120 "$ROUNDTRIP/run.sh" --image ferrofin:test --jellyfin-image jellyfin/jellyfin:test \
    --fixtures "$TMP/fixtures" --work "$TMP/w" --run-id stopped --only 12.4-from-123 > out.txt 2>&1 &
  pid=$!
  for _ in $(seq 150); do grep -qs -- '--name rt-stopped-12.4-from-123-jellyfin ' state/calls && break; sleep 0.2; done
  grep -q -- '--name rt-stopped-12.4-from-123-jellyfin ' state/calls   # it is waiting on a Jellyfin that never comes up
  kill -TERM "$pid"
  status=0; wait "$pid" || status=$?
  [ "$status" -eq 143 ]
  [ "$(started rt-stopped-)" = "$(removed rt-stopped-)" ]
  [ "$(started rt-stopped- | wc -l)" -eq 2 ]
  grep -qx 'rmi ferrofin-roundtrip:stopped' state/calls
  [ -z "$(compgen -G "$TMP/state/*.pid" || true)" ]   # no fake server left running
  [ -f "$TMP/w/report.html" ]
}

@test "run.sh: a Jellyfin that never answers is given up at ROUNDTRIP_READY_TIMEOUT, one that exits is named with its status" {
  FAKE_JELLYFIN=hang ROUNDTRIP_READY_TIMEOUT=2 run run_gate "$TMP/w1" --only 12.4-from-123
  [ "$status" -eq 1 ]
  [[ "$output" == *"FAIL  12.4-from-123  never became ready within 2 s"* ]]
  FAKE_JELLYFIN=exits run run_gate "$TMP/w2" --only 12.4-from-123
  [ "$status" -eq 1 ]
  [[ "$output" == *"FAIL  12.4-from-123  exited with status 3 before it answered"* ]]
}
