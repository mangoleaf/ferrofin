#!/usr/bin/env bats
# Tests of parity/run.sh: its refusals, the cap, the teardown and the failure reports.
# A fake `docker` starts a fake server (fake_server.py) in place of each container, so no image,
# corpus or network is needed. The comparison itself is tested in test_diff.py.

setup() {
  PARITY=$(cd "$BATS_TEST_DIRNAME/.." && pwd)
  TMP=$BATS_TEST_TMPDIR
  cd "$TMP" || return 1
  export FAKE_STATE=$TMP/docker-state
  mkdir -p "$FAKE_STATE" bin
  unset PARITY_CAP_S PARITY_REQUEST_TIMEOUT PARITY_PIN FAKE_DIFF FAKE_JQ_FAIL FAKE_RUN_DELAY
  # The fakes follow the real pin, so moving UPSTREAM_TAG breaks no test: the latest release is the
  # pin, and the fake servers report that release's version.
  export FAKE_LATEST_TAG; FAKE_LATEST_TAG=$(python3 -c "import sys; sys.path.insert(0, '$PARITY/../bench'); import oracle; print(oracle.upstream_tag())")
  export FAKE_VERSION=${FAKE_LATEST_TAG#v}.0
  export PARITY_PIN=0 READY_TIMEOUT_S=8 DRAIN_TIMEOUT_S=8 HTTP_TIMEOUT_S=5 CLEANUP_TIMEOUT_S=5
  read -r ORACLE_PORT FERROFIN_PORT < <(python3 -c "
import socket
sockets = [socket.socket() for _ in range(2)]
for s in sockets: s.bind(('127.0.0.1', 0))
print(*[s.getsockname()[1] for s in sockets])")
  export PARITY_ORACLE_PORT=$ORACLE_PORT PARITY_FERROFIN_PORT=$FERROFIN_PORT
  PIN=$(cat "$PARITY/../bench/testdata/jellyfin-image.txt")
  fake_binaries
  testdata td
}

teardown() {
  # nothing a failed test started may outlive it
  [ ! -f "$TMP/leader" ] || kill -TERM -- "-$(cat "$TMP/leader")" 2>/dev/null || true
  pkill -f "fake_server.py $PARITY_ORACLE_PORT " 2>/dev/null || true
  pkill -f "fake_server.py $PARITY_FERROFIN_PORT " 2>/dev/null || true
}

# docker that runs a fake server per container (the image name picks its behaviour), and a git
# whose ls-remote lists $FAKE_LATEST_TAG as the latest Jellyfin release instead of asking the network.
fake_binaries() {
  cat > bin/docker <<EOS
#!/usr/bin/env bash
state=$FAKE_STATE
echo "\$*" >> "\$state/calls"
sanitise() { echo "\${1//[^A-Za-z0-9]/-}"; }
start_server() { # start_server NAME: (re)start the fake server recorded for it
  local name=\$1 port behaviour
  port=\$(cat "\$state/\$name.port"); behaviour=\$(cat "\$state/\$name.behaviour")
  setsid python3 "$PARITY/tests/fake_server.py" "\$port" "\$behaviour" >/dev/null 2>&1 &
  echo \$! > "\$state/\$name.pid"
}
stop_server() { [ ! -f "\$state/\$1.pid" ] || { kill "\$(cat "\$state/\$1.pid")" 2>/dev/null || true; rm -f "\$state/\$1.pid"; }; }
case "\$1" in
  image)
    case "\$*" in *missing*) exit 1;; esac
    if [ "\$2" = inspect ] && [ "\$3" = -f ]; then echo "sha256:\$(sanitise "\$5")"; fi
    exit 0;;
  ps) ls "\$state" 2>/dev/null | sed -n 's/\.name\$//p'; exit 0;;
  run)
    args=("\$@"); image=\${args[-1]}; name=""; port=""; cid=""
    for ((i = 0; i < \${#args[@]}; i++)); do
      case "\${args[i]}" in --name) name=\${args[i+1]};; --cidfile) cid=\${args[i+1]};; -p) port=\${args[i+1]#127.0.0.1:}; port=\${port%%:*};; esac
    done
    case "\$image" in *wontstart*) echo "fake docker: cannot start" >&2; exit 125;; esac
    sleep "\${FAKE_RUN_DELAY:-0}"   # a slow create: the container exists only once this returns
    behaviour=same
    case "\$image" in *nulled*) behaviour=nulled;; *slow*) behaviour=slow;; *quiet*) behaviour=quiet;; *leaky*) behaviour=leaky;; esac
    echo "\$name" > "\$state/\$name.name"; echo "\$port" > "\$state/\$name.port"; echo "\$behaviour" > "\$state/\$name.behaviour"
    echo "\$name" > "\$cid"; start_server "\$name"; echo "\$name"; exit 0;;
  stop) stop_server "\${@: -1}"; exit 0;;
  start) start_server "\$2"; exit 0;;
  inspect) [ -f "\$state/\${@: -1}.name" ] && exit 0; exit 1;;
  logs) echo "fake server log"; exit 0;;
  rm) name=\${@: -1}; stop_server "\$name"; rm -f "\$state/\$name".*; exit 0;;
esac
echo "unexpected docker \$*" >&2; exit 99
EOS
  cat > bin/git <<EOS
#!/usr/bin/env bash
if [ "\$1" = ls-remote ]; then
  [ -n "\${FAKE_LATEST_TAG:-}" ] || exit 128
  printf 'abc\trefs/tags/%s\n' "\$FAKE_LATEST_TAG"; exit 0
fi
exec $(command -v git) "\$@"
EOS
  # a jq that fails when the run's own metadata is written (FAKE_JQ_FAIL=<status>): an unexpected abort
  cat > bin/jq <<EOS
#!/usr/bin/env bash
if [ -n "\${FAKE_JQ_FAIL:-}" ] && [[ "\$*" == *"--argjson started"* ]]; then exit "\$FAKE_JQ_FAIL"; fi
exec $(type -ap jq | grep -v /shims/ | head -n1) "\$@"   # skip a version-manager shim, which would find this one again
EOS
  # a python3 whose comparison step can be made to hang or crash (FAKE_DIFF=hang|crash)
  cat > bin/python3 <<EOS
#!/usr/bin/env bash
case "\$*" in
  *"diff.py run"*)
    case "\${FAKE_DIFF:-}" in
      hang) exec sleep 100;;
      crash) echo "Traceback: the comparison blew up" >&2; exit 2;;
    esac;;
esac
exec $(command -v python3) "\$@"
EOS
  chmod +x bin/docker bin/git bin/python3 bin/jq
  PATH="$TMP/bin:$PATH"
}

# testdata DIR: a corpus the runner accepts, with a preparation that matches the fake oracle image
testdata() {
  local d=$1 image=${2:-$PIN}
  mkdir -p "$d/config/data" "$d/config/root/default/Movies" "$d/media" "$d/jellyfin/config/root/default/Movies"
  echo db > "$d/config/data/jellyfin.db"
  echo '<LibraryOptions/>' | tee "$d/config/root/default/Movies/options.xml" > "$d/jellyfin/config/root/default/Movies/options.xml"
  echo '{"user": "u1", "user_name": "bench", "password": "pw", "movie": "m1"}' > "$d/ids.json"
  jq -n --arg id "sha256:$(echo "$image" | tr -c 'A-Za-z0-9\n' '-')" --arg db "$(sha256sum "$d/config/data/jellyfin.db" | cut -d' ' -f1)" \
    --arg ids "$(sha256sum "$d/ids.json" | cut -d' ' -f1)" --arg version "$FAKE_VERSION" \
    '{image_id: $id, source_db_sha256: $db, ids_sha256: $ids, validation: {system_info: {Version: $version}}}' > "$d/jellyfin/preparation.json"
}

parity() { # parity ARGS...: the runner against the fake corpus in td, the pinned oracle and a fake Ferrofin image
  "$PARITY/run.sh" --testdata td --out out --oracle "$PIN" --ferrofin "${FERROFIN_IMAGE:-ferrofin:ok}" "$@"
}

containers_gone() { # no container left in the fake docker, no server process left on either port
  if compgen -G "$FAKE_STATE/*.name" >/dev/null; then echo "containers left: $(ls "$FAKE_STATE")" >&2; return 1; fi
  [ -z "$(pgrep -f "fake_server.py ($PARITY_ORACLE_PORT|$PARITY_FERROFIN_PORT) " || true)" ]
}

# ── refusals: nothing boots, no run directory is left ───────────────────────────────────────────
@test "run.sh: both images are required" {
  run "$PARITY/run.sh" --oracle "$PIN"
  [ "$status" -eq 2 ]
  [[ "$output" == *"--oracle IMAGE and --ferrofin IMAGE are required"* ]]
}

@test "run.sh: an option without its value, and a bad cap or port pair, are refused" {
  run "$PARITY/run.sh" --oracle
  [ "$status" -eq 2 ]
  [[ "$output" == *"--oracle needs a value"* ]]
  PARITY_CAP_S=7201 run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"at most 7200"* ]]
  PARITY_FERROFIN_PORT=$ORACLE_PORT run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"ports are different"* ]]
  PARITY_ORACLE_PORT=70000 run parity
  [ "$status" -eq 2 ]
}

@test "run.sh: an unknown option and an image docker lacks are refused" {
  run parity --bogus
  [ "$status" -eq 2 ]
  FERROFIN_IMAGE=ferrofin:missing run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"image ferrofin:missing missing"* ]]
  [ ! -e out ]
}

@test "run.sh: the timeout, the cap and the pin setting are held to their ranges" {
  PARITY_REQUEST_TIMEOUT=9 run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"10 to 120"* ]]
  PARITY_REQUEST_TIMEOUT=121 run parity
  [ "$status" -eq 2 ]
  PARITY_CAP_S=0 run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"PARITY_CAP_S must be a positive integer"* ]]
  PARITY_PIN=2 run parity
  [ "$status" -eq 2 ]
  [ ! -e out ]
}

@test "run.sh: a ledger entry without a link fails the run before anything boots" {
  mkdir repo && cp -r "$PARITY" repo/parity && ln -s "$PARITY/../bench" repo/bench   # the layout run.sh finds its helpers in
  printf 'version = 1\n[[entry]]\nrequest = "a"\npath = "X"\nreason = "no link"\n' > repo/parity/ledger.toml
  run repo/parity/run.sh --testdata td --out out --oracle "$PIN" --ferrofin ferrofin:ok
  [ "$status" -eq 2 ]
  [[ "$output" == *"exactly one link"* ]]
  [ ! -e out ]
  [ ! -e "$FAKE_STATE/calls" ]   # docker was never called
}

@test "run.sh: a corpus without the browse user, or a request template the ids cannot fill, is refused" {
  echo '{"movie": "m1"}' > td/ids.json
  run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"lacks the browse user"* ]]
  testdata td
  echo '{"id": "a", "method": "GET", "path": "/Items/{nothing}"}' > bad.jsonl
  run parity --requests bad.jsonl
  [ "$status" -eq 2 ]
  [[ "$output" == *"no \`nothing\`"* ]]
  [ ! -e "$FAKE_STATE/calls" ] || [ "$(grep -c '^run ' "$FAKE_STATE/calls" || true)" = 0 ]
}

@test "run.sh: a pinned run needs one even range of server cpus, and a container left over from another run is refused" {
  PARITY_PIN=1 SERVER_CPUS=8,9 run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"one range"* ]]
  PARITY_PIN=1 SERVER_CPUS=8-10 run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"even number"* ]]
  touch "$FAKE_STATE/parity-old.name"
  run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"a parity-* container exists"* ]]
  [ ! -e out ]
}

@test "run.sh: a corpus that was not prepared for this oracle image is refused" {
  testdata td other-oracle-image
  run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"preparation does not match"* ]]
  rm td/jellyfin/preparation.json
  run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"prepare Jellyfin first"* ]]
}

@test "run.sh: a gate run refuses a stale oracle pin and an oracle that is not the pin; an ad-hoc run only warns" {
  PARITY_PIN=1 FAKE_LATEST_TAG=v99.0 run parity --gate
  [ "$status" -eq 2 ]
  [[ "$output" == *"refusing"* ]]
  [ ! -e out ]
  FAKE_LATEST_TAG=v99.0 run parity
  [ "$status" -eq 0 ]
  [[ "$output" == *"warning"* ]]
  rm -rf out
  testdata td other:oracle
  PARITY_PIN=1 run "$PARITY/run.sh" --testdata td --out out --oracle other:oracle --ferrofin ferrofin:ok --gate
  [ "$status" -eq 2 ]
  [[ "$output" == *"is not the pinned oracle"* ]]
}

@test "run.sh: a release-gate run is pinned, and refused before the network is asked anything" {
  unset FAKE_LATEST_TAG   # the fake git would refuse the pin lookup: this must fail first
  PARITY_PIN=0 run parity --gate
  [ "$status" -eq 2 ]
  [[ "$output" == *"release-gate run is pinned"* ]]
}

@test "run.sh: a corpus on a network filesystem is refused" {
  printf '#!/usr/bin/env bash\necho nfs4\n' > bin/findmnt; chmod +x bin/findmnt
  run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"network filesystem (nfs4)"* ]]
  [ ! -e out ]
}

# ── the run ───────────────────────────────────────────────────────────────────────────────────
@test "run.sh: servers that agree pass; the report and run.json are written and both containers are removed" {
  run parity
  [ "$status" -eq 0 ]
  [ "$(jq -r .verdict out/results.json)" = pass ]
  [ "$(jq -r '.requests | length' out/results.json)" = 1 ]
  grep -q "PASS" out/report.html
  [ "$(jq -r .cap_s out/run.json)" = 7200 ]
  [ "$(jq -r .ferrofin.image out/run.json)" = ferrofin:ok ]
  [ "$(jq -r .oracle.version out/run.json)" = "$FAKE_VERSION" ]
  grep -q '^rm -f parity-oracle' "$FAKE_STATE/calls"
  grep -q '^rm -f parity-ferrofin' "$FAKE_STATE/calls"
  [ -f out/oracle/server.log ]
  containers_gone
}

@test "run.sh: the servers' media is mounted read-only and each runs on a copy of the corpus" {
  run parity
  [ "$status" -eq 0 ]
  grep -q -- "-v $TMP/td/media:/media:ro" "$FAKE_STATE/calls"
  grep -q -- "-v $TMP/out/oracle/config:/config" "$FAKE_STATE/calls"
  grep -q -- "-v $TMP/out/ferrofin/config:/config" "$FAKE_STATE/calls"
  grep -q 'FERROFIN_DATA_DIR=/config' "$FAKE_STATE/calls"
  # the library copies stopped watching the filesystem; the corpus itself is untouched
  grep -q '<EnableRealtimeMonitor>false' out/oracle/config/root/default/Movies/options.xml
  [ "$(grep -c 'EnableRealtimeMonitor' td/config/root/default/Movies/options.xml || true)" = 0 ]
}

@test "run.sh: a field Ferrofin nulls fails the run, and the report names it" {
  FERROFIN_IMAGE=ferrofin:nulled run parity
  [ "$status" -eq 1 ]
  [ "$(jq -r .verdict out/results.json)" = "new differences" ]
  grep -q "Overview" out/report.html
  grep -q "NEW DIFFERENCES" out/report.html
  containers_gone
}

@test "run.sh: a run directory is never reused" {
  mkdir out
  run parity
  [ "$status" -eq 2 ]
  [[ "$output" == *"never reused"* ]]
}

@test "run.sh: a server that does not start fails the run with a report, and the other one is removed" {
  FERROFIN_IMAGE=ferrofin:wontstart run parity
  [ "$status" -eq 1 ]
  grep -q "RUN FAILED" out/report.html
  grep -q "docker could not start" out/report.html
  [ "$(jq -r .verdict out/results.json)" = "run failed" ]
  grep -q '^rm -f parity-oracle' "$FAKE_STATE/calls"
  containers_gone
}

@test "run.sh: a WASM plugin in the corpus copy fails the run: the answers are the core servers'" {
  mkdir -p td/jellyfin/config/plugins && touch td/jellyfin/config/plugins/x.wasm
  run parity
  [ "$status" -eq 1 ]
  grep -q "WASM plugin" out/report.html
  containers_gone
}

@test "run.sh: an oracle that is not the release the corpus was prepared with fails the run" {
  jq '.validation.system_info.Version = "1.0.0"' td/jellyfin/preparation.json > prep.json && mv prep.json td/jellyfin/preparation.json
  run parity
  [ "$status" -eq 1 ]
  grep -q "expected Jellyfin 1.0.0" out/report.html
  containers_gone
}

@test "run.sh: a server whose automatic tasks cannot be switched off fails the run" {
  FERROFIN_IMAGE=ferrofin:leaky run parity
  [ "$status" -eq 1 ]
  grep -q "automatic task triggers remain enabled" out/report.html
  containers_gone
}

@test "run.sh: a server that never answers fails the run, naming the ping" {
  READY_TIMEOUT_S=3 FERROFIN_IMAGE=ferrofin:quiet run parity
  [ "$status" -eq 1 ]
  grep -q "System/Ping" out/report.html
  containers_gone
}

# ── the cap ───────────────────────────────────────────────────────────────────────────────────
@test "run.sh: past the cap the run stops, fails with a named reason, writes a partial report and removes both containers" {
  printf '%s\n' '{"id": "a", "method": "GET", "path": "/Users/{user}/Items/{movie}"}' \
                '{"id": "b", "method": "GET", "path": "/Users/{user}/Items/{movie}"}' > two.jsonl
  start=$SECONDS
  PARITY_CAP_S=20 PARITY_REQUEST_TIMEOUT=10 FERROFIN_IMAGE=ferrofin:slow run parity --requests two.jsonl
  [ "$status" -eq 1 ]
  [ "$(jq -r .verdict out/results.json)" = "run failed" ]
  grep -q "before every request was sent" out/report.html   # the comparison hit the cap, not the boot
  [ "$(jq -r '.requests | length' out/results.json)" -lt 2 ]   # the request the cap cut short is not among them
  [ $((SECONDS - start)) -lt 45 ]   # the slow server would have held it for 60 s per request
  containers_gone
}

@test "run.sh: a comparison that hangs past the cap is killed by the backstop, and the run fails with a report" {
  PARITY_CAP_S=16 CLEANUP_TIMEOUT_S=2 FAKE_DIFF=hang run parity
  [ "$status" -eq 1 ]
  grep -q "RUN FAILED" out/report.html
  grep -q "was killed" out/report.html
  containers_gone
}

@test "run.sh: a comparison that crashes fails the run (exit 1, not the crash's own code) and the report keeps the message" {
  FAKE_DIFF=crash run parity
  [ "$status" -eq 1 ]
  grep -q "the comparison blew up" out/report.html
  containers_gone
}

@test "run.sh: an unexpected abort exits 1, not the failing command's own status, and still leaves a report" {
  FAKE_JQ_FAIL=3 run parity
  [ "$status" -eq 1 ]
  grep -q "RUN FAILED" out/report.html
  containers_gone
}

# ── teardown ──────────────────────────────────────────────────────────────────────────────────
@test "run.sh: a termination while a container is still being created leaves no container behind" {
  FAKE_RUN_DELAY=8 setsid "$PARITY/run.sh" --testdata td --out out --oracle "$PIN" --ferrofin ferrofin:ok &
  leader=$!
  echo "$leader" > "$TMP/leader"
  for _ in $(seq 1 40); do [ -f "$FAKE_STATE/calls" ] && grep -q '^run ' "$FAKE_STATE/calls" && break; sleep 0.25; done
  grep -q '^run ' "$FAKE_STATE/calls"   # docker run is in flight
  asked=$SECONDS
  kill -TERM -- "-$leader"
  wait "$leader" || true
  [ $((SECONDS - asked)) -lt 5 ]        # the create is stopped, not waited out (it would take 8 s)
  sleep 9                               # longer than the delay: a leaked create would have registered by now
  [ -f out/report.html ]
  containers_gone
}

@test "run.sh: a hangup (a closed terminal) removes both containers, leaves a failure report and exits 129" {
  PARITY_CAP_S=300 setsid "$PARITY/run.sh" --testdata td --out out --oracle "$PIN" --ferrofin ferrofin:slow &
  leader=$!
  echo "$leader" > "$TMP/leader"
  for _ in $(seq 1 60); do [ -f out/ferrofin/boot.json ] && [ -f out/oracle/boot.json ] && break; sleep 0.5; done
  [ -f out/oracle/boot.json ]
  sleep 1
  kill -HUP -- "-$leader"
  status=0; wait "$leader" || status=$?
  [ "$status" -eq 129 ]
  grep -q "RUN FAILED" out/report.html
  containers_gone
}

@test "run.sh: a termination mid-comparison removes both containers and leaves a failure report" {
  PARITY_CAP_S=300 setsid "$PARITY/run.sh" --testdata td --out out --oracle "$PIN" --ferrofin ferrofin:slow &
  leader=$!
  echo "$leader" > "$TMP/leader"   # teardown stops the group if an assertion below fails first
  for _ in $(seq 1 60); do [ -f out/ferrofin/boot.json ] && [ -f out/oracle/boot.json ] && break; sleep 0.5; done
  [ -f out/oracle/boot.json ]
  sleep 1
  kill -TERM -- "-$leader"   # an asynchronous job ignores SIGINT; a terminal ^C reaches the foreground run the same way
  wait "$leader" || true
  [ -f out/report.html ]
  grep -q "RUN FAILED" out/report.html
  grep -q '^rm -f parity-oracle' "$FAKE_STATE/calls"
  grep -q '^rm -f parity-ferrofin' "$FAKE_STATE/calls"
  containers_gone
}
