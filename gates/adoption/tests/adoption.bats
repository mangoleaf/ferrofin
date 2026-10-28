#!/usr/bin/env bats
# Tests for the adoption harness (gates/adoption/lib.sh, run.sh, smoke.sh, build-fixtures.sh).
# Nothing here needs docker, a Jellyfin database or external services: tests use canned
# logs and smoke outputs, throwaway SQLite files and a local HTTP server.

bats_require_minimum_version 1.5.0   # run --separate-stderr

setup() {
  ADOPTION="$BATS_TEST_DIRNAME/.."
  # shellcheck source=gates/adoption/lib.sh
  . "$ADOPTION/lib.sh"
  TMP="$BATS_TEST_TMPDIR/fixtures"
  mkdir -p "$TMP"
  cd "$TMP" || exit 1
  unset FERROFIN_ADOPTION_FIXTURES FERROFIN_GATES_DIR IMAGE ADOPTION_USER ADOPTION_WORK_DIR ADOPTION_SCAN_TIMEOUT ADOPTION_ORACLE ORACLE_VERSION
}

# A smoke output in the real format: "status  metric  path".
smoke() { # smoke <file> <episode-count> [playlists-view-listed]
  cat > "$1" <<EOS
200  12.2.0 Jellyfin Server                        /System/Info/Public
200  synthetic-admin admin=true                              /Users/Me
200  Movies,${3:+Playlists,}TV                     /Users/dddddddddddddddddddddddddddddddd/Views
200  $2                                            /Items?UserId=dddddddddddddddddddddddddddddddd&Recursive=true&IncludeItemTypes=Episode&Limit=0
200  26 running=                                   /ScheduledTasks
200  (bytes=$RANDOM)                               /Items/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/Images/Primary?maxWidth=200
EOS
}

# A boot log in Ferrofin's JSON line format.
boot_log() { # boot_log <file> <generation> [extra-line…]
  {
    echo "{\"level\":\"INFO\",\"fields\":{\"message\":\"adopted an existing Jellyfin database in place\",\"generation\":\"$2\"}}"
    echo '{"level":"INFO","fields":{"message":"database migrations applied","migrations":34,"head":34}}'
    shift 2; for line in "$@"; do echo "$line"; done
  } > "$1"
}

# --- normalise ---------------------------------------------------------------

@test "normalise: redacts ids, drops image byte counts and ignored paths" {
  smoke a.txt 120; smoke b.txt 120
  run diff <(adoption_normalise a.txt) <(adoption_normalise b.txt)
  [ "$status" -eq 0 ]
  run adoption_normalise a.txt
  [[ "$output" == *"/Users/<id>/Views"* ]]
  [[ "$output" == *"(bytes)"* ]]
  [[ "$output" != *"/ScheduledTasks"* ]]
  [[ "$output" != *"/System/Info"* ]]
}

@test "normalise: a real difference survives" {
  smoke a.txt 120; smoke b.txt 119
  run diff <(adoption_normalise a.txt) <(adoption_normalise b.txt)
  [ "$status" -eq 1 ]
}

# --- boot log ----------------------------------------------------------------

@test "check_boot_log: a clean boot of the expected generation says nothing" {
  boot_log boot.log 12.2.0
  run adoption_check_boot_log boot.log 12.2.0
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "check_boot_log: the wrong generation is named with what was expected" {
  boot_log boot.log 10.11.8
  run adoption_check_boot_log boot.log 10.11.11
  [ "$output" = "generation '10.11.8' != '10.11.11'" ]
}

@test "check_boot_log: missing migrations line and an ERROR are both reported" {
  echo '{"level":"ERROR","fields":{"message":"boom"}}' > boot.log
  run adoption_check_boot_log boot.log 12.0.0
  [ "${lines[0]}" = "generation '' != '12.0.0'" ]
  [ "${lines[1]}" = "migrations did not complete" ]
  [ "${lines[2]}" = "ERROR in boot log" ]
}

# --- smoke comparison ----------------------------------------------------------

@test "compare_smoke: identical answers pass, a count that moved fails and points at the diff" {
  smoke oracle.txt 120 yes; smoke same.txt 120 yes; smoke fewer.txt 119 yes
  run adoption_compare_smoke oracle.txt same.txt
  [ -z "$output" ]
  ORACLE_VERSION=12.2
  run adoption_compare_smoke oracle.txt fewer.txt
  [ "$output" = "smoke differs from Jellyfin 12.2 (diff oracle.txt fewer.txt)" ]
}

@test "compare_smoke: the message names the oracle release, 12.1 when none is set" {
  smoke oracle.txt 120; smoke fewer.txt 119
  run adoption_compare_smoke oracle.txt fewer.txt
  [ "$output" = "smoke differs from Jellyfin 12.1 (diff oracle.txt fewer.txt)" ]
  ORACLE_VERSION=12.1
  run adoption_compare_smoke oracle.txt fewer.txt
  [ "$output" = "smoke differs from Jellyfin 12.1 (diff oracle.txt fewer.txt)" ]
}

@test "compare_smoke: a skipped probe is left out on both sides, every other line still counts" {
  smoke oracle.txt 120; smoke unscanned.txt 120; smoke fewer.txt 119
  echo '200  2                                             /Persons?UserId=dddddddddddddddddddddddddddddddd&Limit=0' >> oracle.txt
  echo '200  0                                             /Persons?UserId=dddddddddddddddddddddddddddddddd&Limit=0' >> unscanned.txt
  echo '200  0                                             /Persons?UserId=dddddddddddddddddddddddddddddddd&Limit=0' >> fewer.txt
  run adoption_compare_smoke oracle.txt unscanned.txt
  [ "$output" = "smoke differs from Jellyfin 12.1 (diff oracle.txt unscanned.txt)" ]
  run adoption_compare_smoke oracle.txt unscanned.txt '/Persons\?'
  [ -z "$output" ]
  run adoption_compare_smoke oracle.txt fewer.txt '/Persons\?'
  [ "$output" = "smoke differs from Jellyfin 12.1 (diff oracle.txt fewer.txt)" ]
}

@test "compare_smoke: a view that is listed by one server and not the other fails" {
  smoke oracle.txt 120; smoke views.txt 120 yes
  run adoption_compare_smoke oracle.txt views.txt
  [ -n "$output" ]
}

# --- database checks -------------------------------------------------------------

@test "check_db: a consistent file is clean; a dangling foreign key is reported" {
  sqlite3 ok.db 'CREATE TABLE p(id INTEGER PRIMARY KEY); CREATE TABLE c(id INTEGER PRIMARY KEY, p REFERENCES p(id)); INSERT INTO p VALUES (1); INSERT INTO c VALUES (1, 1);'
  run adoption_check_db ok.db
  [ -z "$output" ]
  sqlite3 bad.db 'CREATE TABLE p(id INTEGER PRIMARY KEY); CREATE TABLE c(id INTEGER PRIMARY KEY, p REFERENCES p(id)); INSERT INTO c VALUES (1, 99); INSERT INTO c VALUES (2, 98);'
  run adoption_check_db bad.db
  [ "$output" = "foreign_key_check: 2 rows" ]
}

@test "check_db: a missing database reports both query failures on stdout" {
  run bash -c '. "$1"; adoption_check_db "$2" 2>/dev/null' _ "$ADOPTION/lib.sh" missing.db
  [[ "${lines[0]}" == 'integrity_check: sqlite3 failed:'* ]]
  [[ "${lines[1]}" == 'foreign_key_check: sqlite3 failed:'* ]]
  [ ! -e missing.db ]
}

@test "check_db: an invalid database reports both query failures on stdout" {
  printf '%s\n' 'this is not a SQLite database' > invalid.db
  run bash -c '. "$1"; adoption_check_db "$2" 2>/dev/null' _ "$ADOPTION/lib.sh" invalid.db
  [[ "${lines[0]}" == 'integrity_check: sqlite3 failed:'* ]]
  [[ "${lines[1]}" == 'foreign_key_check: sqlite3 failed:'* ]]
}

# --- second boot ------------------------------------------------------------------

@test "second_boot_repairs: a repair that did work is flagged, one that found nothing is not" {
  cat > second.log <<'EOS'
{"fields":{"message":"database migrations applied"}}
{"fields":{"message":"repaired OwnerId relationships","items":0,"repaired":0}}
{"fields":{"message":"scheduled task started"}}
EOS
  run adoption_second_boot_repairs second.log
  [ "$status" -eq 0 ]
  [ -z "$output" ]
  echo '{"fields":{"message":"imported playlist/collection/version membership from Data JSON","rows":12}}' >> second.log
  run adoption_second_boot_repairs second.log
  [ "$status" -eq 0 ]
  [[ "$output" == *"imported playlist/collection/version membership"* ]]
}

@test "second_boot_repairs: promotions are work even when repaired is zero" {
  echo '{"fields": {"message": "repaired alternate-version primaries from LinkedChildren", "repaired": 0, "promoted": 1}}' > second.log
  run adoption_second_boot_repairs second.log
  [ "$status" -eq 0 ]
  [[ "$output" == *'repaired alternate-version primaries'* ]]
}

@test "second_boot_repairs: all-zero counters are ignored regardless of their order" {
  echo '{"fields": {"promoted": 0, "message": "repaired alternate-version primaries from LinkedChildren", "repaired": 0}}' > second.log
  run adoption_second_boot_repairs second.log
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "second_boot_repairs: a repair without counters is still reported" {
  echo '{"fields":{"message":"repaired OwnerId relationships"}}' > second.log
  run adoption_second_boot_repairs second.log
  [ "$status" -eq 0 ]
  [[ "$output" == *'repaired OwnerId relationships'* ]]
}

@test "second_boot_repairs: reports only the first repair that did work, ignoring log noise" {
  cat > second.log <<'EOS'
not a JSON log line
{"fields":{"message":"scheduled task started","items":12}}
{"fields":{"message":"repaired OwnerId relationships","repaired":0}}
{"fields":{"message":"repaired OwnerId relationships","repaired":2}}
{"fields":{"message":"backfilled other relationships","rows":3}}
EOS
  run adoption_second_boot_repairs second.log
  [ "$status" -eq 0 ]
  [ "${#lines[@]}" -eq 1 ]
  [ "$output" = '{"fields":{"message":"repaired OwnerId relationships","repaired":2}}' ]
}

# --- credentials ----------------------------------------------------------------------

seed_users_db() {
  sqlite3 users.db <<'EOS'
CREATE TABLE Users (Id TEXT PRIMARY KEY, Username TEXT);
CREATE TABLE Permissions (UserId TEXT, Kind INTEGER, Value INTEGER);
CREATE TABLE Devices (UserId TEXT, AccessToken TEXT, DeviceId TEXT, AppName TEXT, DateLastActivity TEXT);
INSERT INTO Users VALUES ('AAAA-1', 'synthetic-admin'), ('BBBB-2', 'shared');
INSERT INTO Permissions VALUES ('AAAA-1', 0, 1), ('BBBB-2', 0, 0);
INSERT INTO Devices VALUES ('AAAA-1', 'tok-synthetic-admin', 'dev-synthetic-admin', 'Jellyfin Web', '2026-01-01');
INSERT INTO Devices VALUES ('AAAA-1', 'tok-old',   'dev-old',   'Jellyfin Web', '2025-01-01');
INSERT INTO Devices VALUES ('BBBB-2', 'tok-shared','dev-shared','Jellyfin Web', '2026-09-01');
INSERT INTO Devices VALUES ('BBBB-2', 'tok-tv',    'dev-tv',    'Wholphin',     '2026-09-15');
EOS
}

@test "smoke_credentials: defaults to the administrator's newest web session, not the most recent user" {
  seed_users_db
  run adoption_smoke_credentials users.db
  [ "$status" -eq 0 ]
  [ "$output" = "synthetic-admin tok-synthetic-admin dev-synthetic-admin aaaa1" ]
}

@test "smoke_credentials: a named user is honoured even when not an administrator" {
  seed_users_db
  run adoption_smoke_credentials users.db shared
  [ "$output" = "shared tok-shared dev-shared bbbb2" ]
}

@test "smoke_credentials: no web session is a failure that names the account" {
  seed_users_db
  run adoption_smoke_credentials users.db nobody
  [ "$status" -eq 1 ]
  [[ "$output" == *"no Jellyfin Web session for nobody"* ]]
}

# --- run.sh command line -----------------------------------------------------------

# copy_gates: gates/ copied into a repository of its own, its adoption/ in COPY. gates/lib.sh refuses an
# output root inside the repository it runs from, so a root under the test's directory is refused when
# TMPDIR itself is inside this checkout (the pre-push CI mirror keeps scratch files there): from the copy
# it is beside the repository, as it would be on a real host.
copy_gates() {
  mkdir -p "$BATS_TEST_TMPDIR/repo" && cp -r "$ADOPTION/.." "$BATS_TEST_TMPDIR/repo/gates" && git -C "$BATS_TEST_TMPDIR/repo" init -q
  COPY=$BATS_TEST_TMPDIR/repo/gates/adoption
}

copy_fixtures_dir() { # the copy's adoption_fixtures_dir (the same name: the copy's files are the same)
  # shellcheck disable=SC2016  # expanded by the inner bash
  bash -c '. "$1"; adoption_fixtures_dir test' _ "$COPY/lib.sh"
}

one_row_fixture() { # one_row_fixture <dir>: the smallest fixture run.sh adopts (one messy row)
  mkdir -p "$1/oracle" "$1/media" "$1/jellyfin-12.2-messy/data"
  smoke "$1/oracle/smoke-jellyfin-12.2.txt" 120
  empty_adoption_db "$1/jellyfin-12.2-messy/data/jellyfin.db"
  printf '{"lineage": "12.x", "shapes": {}}' > "$1/jellyfin-12.2-messy/messy.json"
  printf '{"shapes": {}}' > "$1/jellyfin-12.2-messy/messy-oracle.json"
}

bin_first() { # the fake binaries in $TMP/bin win over the real ones (a function: shellcheck reads a @test body as a subshell)
  PATH="$TMP/bin:$PATH"
}

fake_docker() { # a docker on PATH that has every image and no containers
  mkdir -p bin; cat > bin/docker <<'EOS'
#!/usr/bin/env bash
case "$1 $2" in "image inspect") exit 0;; esac
echo "unexpected docker $*" >&2; exit 99
EOS
  chmod +x bin/docker; PATH="$TMP/bin:$PATH"
}

@test "run.sh, build-fixtures.sh and synthetic.py: with no output root and no --fixtures, one line naming FERROFIN_GATES_DIR" {
  for cmd in "$ADOPTION/run.sh" "$ADOPTION/build-fixtures.sh" "python3 $ADOPTION/synthetic.py run"; do
    run $cmd
    [ "$status" -eq 2 ]
    [ "${#lines[@]}" -eq 1 ]
    [[ "$output" == *"set FERROFIN_GATES_DIR"* ]]
  done
}

@test "run.sh: with only FERROFIN_GATES_DIR, the fixtures and the work directory come from the root; --fixtures and ADOPTION_WORK_DIR win" {
  copy_gates
  export FERROFIN_GATES_DIR="$BATS_TEST_TMPDIR/root"
  before=$(find "$BATS_TEST_TMPDIR" -mindepth 1 -maxdepth 1 | wc -l)
  dir=$(copy_fixtures_dir)
  [[ "$dir" == "$FERROFIN_GATES_DIR"/fixtures/adoption-???????????? ]]   # named for the recipe
  run "$COPY/run.sh" --image any
  [ "$status" -eq 2 ]
  [[ "$output" == *"$dir missing — run gates/adoption/build-fixtures.sh first"* ]]
  one_row_fixture "$dir"; one_row_fixture fixtures
  mkdir -p bin; cat > bin/docker <<'EOS'
#!/usr/bin/env bash
case "$1" in
  image|run|restart|rm|logs) exit 0;;
  inspect) echo false; exit 0;;
esac
echo "unexpected docker $*" >&2; exit 99
EOS
  chmod +x bin/docker; bin_first
  run "$COPY/run.sh" --image any --only jellyfin-12.2-messy
  [ "$status" -eq 1 ]
  [ -f "$FERROFIN_GATES_DIR/work/adoption/report.html" ]
  [ -d "$FERROFIN_GATES_DIR/work/adoption/jellyfin-12.2-messy" ]   # the copy, too
  [ ! -e "$dir/work" ]
  [ "$(find "$BATS_TEST_TMPDIR" -mindepth 1 -maxdepth 1 | wc -l)" -eq $((before + 1)) ]   # nothing new beside the root itself
  # --fixtures names the fixtures; the work directory stays the root's
  rm -rf "$FERROFIN_GATES_DIR/work"
  run "$COPY/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-messy
  [ "$status" -eq 1 ]
  [ -f "$FERROFIN_GATES_DIR/work/adoption/report.html" ]
  [ ! -e fixtures/work ]
  # ADOPTION_WORK_DIR names the work directory
  ADOPTION_WORK_DIR="$TMP/mine" run "$COPY/run.sh" --image any --only jellyfin-12.2-messy
  [ "$status" -eq 1 ]
  [ -f mine/report.html ]
  # FERROFIN_ADOPTION_FIXTURES names them as --fixtures does, for synthetic.py too
  FERROFIN_ADOPTION_FIXTURES="$TMP/elsewhere" run "$COPY/run.sh" --image any
  [[ "$output" == *"$TMP/elsewhere missing"* ]]
  unset FERROFIN_GATES_DIR
  FERROFIN_ADOPTION_FIXTURES="$TMP/elsewhere" run python3 "$COPY/synthetic.py" run --image any
  [[ "$output" != *FERROFIN_GATES_DIR* ]]
  [[ "$output" == *"synthetic adoption failed"* ]]
  # without a root, --fixtures keeps its work beside it, as before the root existed
  run "$COPY/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-messy
  [ "$status" -eq 1 ]
  [ -f fixtures/work/run/report.html ]
}

@test "build-fixtures.sh: with only FERROFIN_GATES_DIR it builds into the root's fixtures; --fixtures wins" {
  mkdir -p bin; printf '#!/usr/bin/env bash\nexit 1\n' > bin/docker; chmod +x bin/docker; bin_first   # stops at the first pull
  copy_gates
  root="$BATS_TEST_TMPDIR/root"
  before=$(find "$BATS_TEST_TMPDIR" -mindepth 1 -maxdepth 1 | wc -l)
  FERROFIN_GATES_DIR=$root run "$COPY/build-fixtures.sh"
  [ "$status" -ne 0 ]
  [ -d "$(FERROFIN_GATES_DIR=$root copy_fixtures_dir)" ]
  [ "$(find "$BATS_TEST_TMPDIR" -mindepth 1 -maxdepth 1 | wc -l)" -eq $((before + 1)) ]   # nothing beside the root
  rm -r "$root"
  FERROFIN_GATES_DIR=$root run "$COPY/build-fixtures.sh" --fixtures mine
  [ -d mine ]
  [ ! -e "$root" ]
}

@test "adoption_fixtures_dir: the name follows every file that builds the fixtures, as CI's fixture cache key does" {
  mkdir -p tree/gates && cp -r "$ADOPTION" tree/gates/adoption && cp "$ADOPTION/../lib.sh" tree/gates/
  git -C tree init -q   # a repository of its own: the root beside it is outside it, wherever TMPDIR is
  # shellcheck disable=SC2016  # expanded by the inner bash
  key() { FERROFIN_GATES_DIR="$BATS_TEST_TMPDIR/root" bash -c '. "$1"; adoption_fixtures_dir test' _ tree/gates/adoption/lib.sh; }
  first=$(key)
  for input in synthetic.py smoke.sh build-fixtures.sh messy.py scale.py; do
    echo "# changed" >> "tree/gates/adoption/$input"
    now=$(key)
    [ "$now" != "$first" ]
    first=$now
  done
  rm tree/gates/adoption/scale.py
  run --separate-stderr key
  [ "$status" -eq 2 ]
  # shellcheck disable=SC2154  # $stderr is set by bats' run --separate-stderr
  [[ "$stderr" == *"test: cannot read the fixture builders in gates/adoption/"* ]]
  # the hashed list is the recipe's files plus the files CI's key adds (.github/workflows/adoption.yml)
  listed=$(declare -f adoption_fixtures_dir | grep -o '[a-z_-]*\.\(py\|sh\)' | sort -u)
  recipe=$(python3 -c 'import ast, sys
tree = ast.parse(open(sys.argv[1]).read())
fn = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == "recipe")
print(*sorted({c.value for c in ast.walk(fn) if isinstance(c, ast.Constant) and str(c.value).endswith(".py")}), sep="\n")' "$ADOPTION/synthetic.py")
  ci=$(grep -o 'sha256sum gates/adoption/.*' "$ADOPTION/../../.github/workflows/adoption.yml" | grep -o '[a-z_-]*\.\(py\|sh\)')
  [ "$listed" = "$(printf '%s\n' "$recipe" "$ci" | sort -u)" ]
}

@test "run.sh: refuses to run without the oracle" {
  fake_docker; mkdir fixtures
  run "$ADOPTION/run.sh" --fixtures fixtures --image any
  [ "$status" -eq 2 ]
  [[ "$output" == *"oracle/smoke-jellyfin-12.2.txt missing"* ]]
}

@test "run.sh: a missing fixture is skipped, named, and does not fail the run" {
  fake_docker; mkdir -p fixtures/oracle fixtures/media; smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0
  [ "$status" -eq 0 ]
  [ "${#lines[@]}" -eq 1 ]
  [[ "${lines[0]}" == SKIP*12.0.0*jellyfin-12.0*"fixture missing" ]]
}

@test "run.sh: every supported release has a row" {
  for f in jellyfin-10.11.8 jellyfin-10.11.9 jellyfin-10.11.10 jellyfin-10.11.11 jellyfin-12.0 jellyfin-12.1-from-10 jellyfin-12.1-from-12; do
    grep -q "|$f|" "$ADOPTION/run.sh"
  done
  for f in jellyfin-12.2-from-10 jellyfin-12.2-from-12 jellyfin-12.2-from-121; do
    grep -q "\"12.2.0|$f|[0-9]*\"" "$ADOPTION/run.sh"
  done
  # eleven paths, each on its own port
  run bash -c "grep -oE '^  \"[0-9.]+\\|jellyfin-[^|]+\\|[0-9]+\"' \"\$1\" | cut -d'|' -f3 | sort | uniq -d" _ "$ADOPTION/run.sh"
  [ -z "$output" ]
  [ "$(grep -E '^  "[0-9.]+\|jellyfin-' "$ADOPTION/run.sh" | grep -vc -e '-messy|' -e '-scale|')" -eq 11 ]
  # and the two messy copies and the scale copy, on ports of their own (the uniqueness check above covers them too)
  [ "$(grep -cE '^  "[0-9.]+\|jellyfin-[^|]*-messy\|' "$ADOPTION/run.sh")" -eq 2 ]
  [ "$(grep -cE '^  "[0-9.]+\|jellyfin-[^|]*-scale\|' "$ADOPTION/run.sh")" -eq 1 ]
}

@test "run.sh and build-fixtures.sh carry a fresh-install 12.2 row that adopts as 12.2.0" {
  # The live row for a database created by a Jellyfin 12.2 first run (not upgraded from
  # 10.11): run.sh adopts it as generation 12.2.0 against its own fresh-install 12.2 oracle,
  # and build-fixtures.sh checks its fresh-install EF history id count (105, fewer than any
  # upgrade route, which never records the retired 10.11-era routines).
  grep -q "\"12.2.0|jellyfin-12.2-fresh|[0-9]*\"" "$ADOPTION/run.sh"
  grep -q '("jellyfin-12.2-fresh", "12.2", None)' "$ADOPTION/synthetic.py"
  grep -q 'FRESH_INSTALL' "$ADOPTION/synthetic.py"
  grep -q '12.2-fresh:105' "$ADOPTION/build-fixtures.sh"
  # A clean first-run install is a distinct library from the seeded routes, so it gets its own
  # Jellyfin 12.2 oracle; build-fixtures.sh builds it and run.sh compares the fresh row to it.
  grep -q '^build_oracle 12.2-fresh jellyfin-12.2-fresh [0-9]* jellyfin/jellyfin:12.2$' "$ADOPTION/build-fixtures.sh"
  grep -q 'smoke-jellyfin-12.2-fresh.txt' "$ADOPTION/run.sh"
  # build_oracle takes the image as a required 4th argument and threads it into the container,
  # so the fresh oracle really is a 12.2 image's answers (not the non-existent :12.2-fresh tag).
  # The full build_oracle path needs a live Jellyfin (readiness probe + smoke.sh), so it is
  # exercised by the live build-fixtures.sh run in the Adoption workflow, not offline here.
  grep -qF "local version=\$1 source=\$FIXTURES/\$2 port=\$3 image=\$4" "$ADOPTION/build-fixtures.sh"
  grep -qF "\"\$image\"" "$ADOPTION/build-fixtures.sh"
}

@test "run.sh: the fresh-install row is skipped, named, when its own oracle is missing" {
  # The shared 12.2 oracle present (so the run starts) but the fresh-install oracle absent:
  # the fresh row alone is skipped and named, not compared against the wrong oracle.
  fake_docker; mkdir -p fixtures/oracle fixtures/media fixtures/jellyfin-12.2-fresh/data
  smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-fresh
  [ "$status" -eq 0 ]
  [[ "${lines[0]}" == SKIP*12.2.0*jellyfin-12.2-fresh*"smoke-jellyfin-12.2-fresh.txt missing"* ]]
}

@test "run.sh: only a 10.11 source compares /Persons, and the whole smoke again, after the scan" {
  grep -qF "case \$expected in 10.11.*) after_scan='/Persons\\?';; *) after_scan=;; esac" "$ADOPTION/run.sh"
  grep -qF "adoption_compare_smoke \"\$oracle\" \"\$dst.smoke.txt\" \"\$after_scan\")" "$ADOPTION/run.sh"
  grep -qF "adoption_compare_smoke \"\$oracle\" \"\$dst.smoke-scan.txt\" | sed 's/^/after scan: /')" "$ADOPTION/run.sh"
}

@test "run.sh: the second-boot log starts at the restarted container's own start, and must hold a server start" {
  # a zone-less `date -u` --since is read as local time: empty west of UTC, the first boot east of it
  run grep -cF "logs --since \"\$(date" "$ADOPTION/run.sh"
  [ "$output" = 0 ]
  grep -qF "docker logs --since \"\$(docker inspect -f '{{.State.StartedAt}}' \"\$name\")\" \"\$name\" > \"\$dst.boot2.log\" 2>&1" "$ADOPTION/run.sh"
  grep -qF "grep -q '\"ferrofin-server starting\"' \"\$dst.boot2.log\" || why+=(\"second boot log has no server start\")" "$ADOPTION/run.sh"
}

@test "run.sh: the oracle is Jellyfin 12.2 unless --oracle or ADOPTION_ORACLE picks 12.1" {
  fake_docker; mkdir -p fixtures/oracle fixtures/media; smoke fixtures/oracle/smoke-jellyfin-12.1.txt 120
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0
  [ "$status" -eq 2 ]
  [[ "$output" == *"oracle/smoke-jellyfin-12.2.txt missing"* ]]
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0 --oracle 12.1
  [ "$status" -eq 0 ]
  [[ "${lines[0]}" == SKIP*jellyfin-12.0*"fixture missing" ]]
  ADOPTION_ORACLE=12.1 run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0
  [ "$status" -eq 0 ]
  # the flag wins over the environment
  ADOPTION_ORACLE=12.1 run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0 --oracle 12.2
  [ "$status" -eq 2 ]
  [[ "$output" == *"oracle/smoke-jellyfin-12.2.txt missing"* ]]
}

@test "run.sh: an unknown oracle release is refused" {
  run "$ADOPTION/run.sh" --fixtures fixtures --oracle 12.0
  [ "$status" -eq 2 ]
  [[ "$output" == *"--oracle must be 12.1 or 12.2, not '12.0'"* ]]
}

@test "run.sh: refuses without the synthetic media" {
  fake_docker; mkdir -p fixtures/oracle; smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0
  [ "$status" -eq 2 ]
  [[ "$output" == *"fixtures/media missing"* ]]
}

fake_findmnt() { # fake_findmnt <fstype[,fstype…]> — every path reports these mount types, one per line
  mkdir -p bin; printf '#!/usr/bin/env bash\nprintf "%%s\\n" %s\n' "${1//,/ }" > bin/findmnt; chmod +x bin/findmnt
}

@test "run.sh: the only media mounted is the fixtures' own media, read-only" {
  run grep -n '^MEDIA=' "$ADOPTION/run.sh"
  [ "${#lines[@]}" -eq 1 ]
  # shellcheck disable=SC2016 # the script's literal source line, not an expansion
  [[ "$output" == *'MEDIA=(-v "$FIXTURES/media:/media:ro")' ]]
  # the messy rows' tree, when it is built, is the only other one, and read-only as well
  run grep -c 'media-messy:/media:ro' "$ADOPTION/run.sh"
  [ "$output" = 1 ]
}

@test "run.sh, build-fixtures.sh and synthetic.py refuse a network filesystem before creating anything" {
  fake_docker; fake_findmnt cifs
  mkdir -p fixtures/oracle fixtures/media; smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  run "$ADOPTION/run.sh" --fixtures fixtures --image any
  [ "$status" -eq 2 ]
  [[ "$output" == *"network filesystem (cifs)"* ]]
  [ ! -e fixtures/work ]
  run "$ADOPTION/build-fixtures.sh" --fixtures new-fixtures
  [ "$status" -eq 2 ]
  [[ "$output" == *"network filesystem (cifs)"* ]]
  [ ! -e new-fixtures ]
  run python3 "$ADOPTION/synthetic.py" run --fixtures other-fixtures
  [ "$status" -eq 1 ]
  [[ "$output" == *"network filesystem (cifs)"* ]]
  [ ! -e other-fixtures ]
}

# --- build-fixtures.sh ------------------------------------------------------------------

@test "build-fixtures.sh: each 12.2 fixture is built by Jellyfin 12.2 on its contract parent" {
  grep -q '("jellyfin-12.2-from-10", "12.2", "jellyfin-10.11.8")' "$ADOPTION/synthetic.py"
  grep -q '("jellyfin-12.2-from-12", "12.2", "jellyfin-12.0")' "$ADOPTION/synthetic.py"
  grep -q '("jellyfin-12.2-from-121", "12.2", "jellyfin-12.1-from-12")' "$ADOPTION/synthetic.py"
  grep -q '^build_oracle 12.1 jellyfin-12.1-from-12 ' "$ADOPTION/build-fixtures.sh"
  grep -q '^build_oracle 12.2 jellyfin-12.2-from-121 ' "$ADOPTION/build-fixtures.sh"
}

built_fixtures() { # every fixture built at the current recipe, carrying its release's EF history id count
  local recipe f
  mkdir -p bin fixtures/media fixtures/oracle
  printf '#!/usr/bin/env bash\necho "unexpected docker $*" >&2; exit 99\n' > bin/docker; chmod +x bin/docker
  PATH="$TMP/bin:$PATH"
  recipe=$(python3 -c 'import sys; sys.path.insert(0, sys.argv[1]); import synthetic; print(synthetic.recipe())' "$ADOPTION")
  for f in 10.11.8:68 10.11.9:68 10.11.10:71 10.11.11:71 12.0:102 12.1-from-10:104 12.1-from-12:105 \
           12.2-from-10:107 12.2-from-12:108 12.2-from-121:109 12.2-fresh:105; do
    mkdir -p "fixtures/jellyfin-${f%:*}/data"
    printf '{"fixture": "jellyfin-%s", "recipe": "%s"}' "${f%:*}" "$recipe" > "fixtures/jellyfin-${f%:*}/synthetic-ready.json"
    sqlite3 "fixtures/jellyfin-${f%:*}/data/jellyfin.db" "CREATE TABLE __EFMigrationsHistory (MigrationId TEXT, ProductVersion TEXT);
      WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ${f#*:}) INSERT INTO __EFMigrationsHistory SELECT i, '9' FROM n;"
  done
  # the messy copies are built, so build-fixtures.sh's messy.py step finds nothing to do
  for f in jellyfin-10.11.11-messy jellyfin-12.2-messy; do
    mkdir -p "fixtures/$f"; echo '{}' > "fixtures/$f/messy-oracle.json"
    python3 -c 'import json, sys; sys.path.insert(0, sys.argv[1]); import messy; print(json.dumps({"shapes": {s: {} for s in messy.SHAPES}}))' \
      "$ADOPTION" > "fixtures/$f/messy.json"
  done
  # and the scale copy, so scale.py's step finds nothing to do either
  mkdir -p fixtures/jellyfin-10.11.11-scale; echo '{}' > fixtures/jellyfin-10.11.11-scale/scale.json
}

@test "build-fixtures.sh: pulls the release of each fixture still to build, and 10.11.8, before building" {
  built_fixtures
  rm fixtures/jellyfin-12.1-from-10/synthetic-ready.json
  cat > bin/docker <<'EOS'
#!/usr/bin/env bash
[ "$1" = pull ] && { echo "$*" >> "$PULLS"; exit 0; }
exit 99
EOS
  PULLS="$TMP/pulls" run "$ADOPTION/build-fixtures.sh" --fixtures fixtures
  [ "$status" -ne 0 ]
  [ "$(cat pulls)" = "$(printf 'pull -q jellyfin/jellyfin:10.11.8\npull -q jellyfin/jellyfin:12.1')" ]
}

@test "build-fixtures.sh: a fixture without its release's EF history id count fails the build" {
  built_fixtures
  sqlite3 fixtures/jellyfin-12.0/data/jellyfin.db "DELETE FROM __EFMigrationsHistory WHERE MigrationId = '1'"
  smoke fixtures/oracle/smoke-jellyfin-12.1.txt 120; smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  run "$ADOPTION/build-fixtures.sh" --fixtures fixtures
  [ "$status" -eq 1 ]
  [[ "$output" == *"jellyfin-12.0 has 101 EF history ids, expected 102"* ]]
  [[ "$output" != *"unexpected docker"* ]]
}

@test "build-fixtures.sh: with every fixture built it only dumps the 12.2 schema and diffs the other routes" {
  built_fixtures
  for f in 12.2-from-10 12.2-from-12 12.2-from-121; do
    sqlite3 "fixtures/jellyfin-$f/data/jellyfin.db" 'CREATE TABLE t(a); CREATE INDEX i ON t(a) WHERE a IS NOT NULL;
      INSERT INTO t VALUES (1); ANALYZE; CREATE TABLE s(id INTEGER PRIMARY KEY AUTOINCREMENT); INSERT INTO s DEFAULT VALUES;'
  done
  sqlite3 fixtures/jellyfin-12.2-from-12/data/jellyfin.db 'CREATE INDEX extra ON t(a);'
  # Every oracle present (the fresh-install one included) so build_oracle early-returns and the
  # run touches no docker; the fresh fixture is left out of the schema diff, so it needs none of
  # the t/s tables above.
  smoke fixtures/oracle/smoke-jellyfin-12.1.txt 120; smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  smoke fixtures/oracle/smoke-jellyfin-12.2-fresh.txt 120
  run "$ADOPTION/build-fixtures.sh" --fixtures fixtures
  [ "$status" -eq 0 ]
  [[ "$output" == *"jellyfin-12.2-schema.sql: 4 CREATE statements"* ]]
  [[ "$output" == *"jellyfin-12.2-from-10: same schema"* ]]
  [[ "$output" == *"jellyfin-12.2-from-12: schema DIFFERS"* ]]
  [[ "$output" != *"unexpected docker"* ]]
  grep -q 'WHERE a IS NOT NULL' fixtures/jellyfin-12.2-schema.sql
  # dump_schema strips every sqlite_stat* (stat1, and stat4 on a STAT4 build) and sqlite_sequence.
  run grep -cE 'sqlite_stat[0-9]+|sqlite_sequence' fixtures/jellyfin-12.2-schema.sql
  [ "$output" = 0 ]
}

@test "metadata: populated fields survive adoption, HTTP presentation and a completed scan" {
  run python3 -m unittest discover -s "$ADOPTION/tests" -p test_metadata.py
  [ "$status" -eq 0 ]
}

@test "watch history: watched flags and video progress survive in database and API" {
  run python3 -m unittest discover -s "$ADOPTION/tests" -p test_watch_history.py
  [ "$status" -eq 0 ]
}

@test "run.sh: an unreadable metadata baseline fails before starting a container" {
  fake_docker
  mkdir -p fixtures/oracle fixtures/media fixtures/jellyfin-12.0/data
  smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  printf 'not a database\n' > fixtures/jellyfin-12.0/data/jellyfin.db
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0
  [ "$status" -eq 1 ]
  [[ "$output" == *"metadata baseline: metadata check could not read"* ]]
  [[ "$output" != *"unexpected docker run"* ]]
  # the copy lands in work/run/, clear of synthetic.py run's work/<fixture>
  [ -d fixtures/work/run/jellyfin-12.0 ]
  [ ! -e fixtures/work/jellyfin-12.0 ]
  # it is a failed row in the report too: the fixture began, and its row says why it never started
  [ "$(jq -r 'select(.event == "row") | [.fixture, .verdict] | join(" ")' fixtures/work/run/results.jsonl)" = "jellyfin-12.0 FAIL" ]
  jq -r 'select(.event == "row") | .detail' fixtures/work/run/results.jsonl | grep -q "metadata baseline"
  grep -q "metadata baseline" fixtures/work/run/report.html
}

@test "run.sh: a broken or missing report module changes neither the verdict, the exit status nor the output" {
  mkdir -p bin fixtures/oracle fixtures/media fixtures/jellyfin-12.2-messy/data
  smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  empty_adoption_db fixtures/jellyfin-12.2-messy/data/jellyfin.db
  printf '{"lineage": "12.x", "shapes": {}}' > fixtures/jellyfin-12.2-messy/messy.json
  printf '{"shapes": {}}' > fixtures/jellyfin-12.2-messy/messy-oracle.json
  cat > bin/docker <<'EOS'
#!/usr/bin/env bash
case "$1" in
  image|run|restart|rm|logs) exit 0;;
  inspect) echo false; exit 0;;
esac
echo "unexpected docker $*" >&2; exit 99
EOS
  chmod +x bin/docker; bin_first
  run --separate-stderr "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-messy
  normal_status=$status; normal_output=$output
  [ "$normal_status" -eq 1 ]
  [ -f fixtures/work/run/report.html ]
  # shellcheck disable=SC2154  # $stderr is set by bats' run --separate-stderr
  [[ "$stderr" != *"report"* ]]   # a run that wrote its page says nothing about it
  # the same run from a copy of the harness whose report module crashes, then from one that has none
  cp -r "$ADOPTION" copy; cp "$ADOPTION/../lib.sh" .   # the gates' shared library, one level up as in gates/
  printf 'raise SystemExit(7)\n' > copy/adoption_report.py
  rm -rf fixtures/work
  run --separate-stderr copy/run.sh --fixtures fixtures --image any --only jellyfin-12.2-messy
  [ "$status" -eq "$normal_status" ]
  [ "$output" = "$normal_output" ]            # stdout is the same
  [[ "$stderr" == *"report: not written"* ]]   # and the missing page is said, on stderr
  [ ! -e fixtures/work/run/report.html ]
  rm copy/adoption_report.py; rm -rf fixtures/work
  run --separate-stderr copy/run.sh --fixtures fixtures --image any --only jellyfin-12.2-messy
  [ "$status" -eq "$normal_status" ]
  [ "$output" = "$normal_output" ]
  [[ "$stderr" == *"report: not written"* ]]
}

@test "synthetic adoption checks detect missing scenarios, changed membership and redundant writes" {
  run python3 -m unittest discover -s "$ADOPTION/tests" -p test_preservation.py
  [ "$status" -eq 0 ]
}

@test "user accounts: avatars, login states and complete settings survive adoption" {
  run python3 -m unittest discover -s "$BATS_TEST_DIRNAME" -p test_user_accounts.py
  [ "$status" -eq 0 ]
}

@test "table digests: a lost row or changed column in any listed table fails; documented rewrites pass" {
  run python3 -m unittest discover -s "$BATS_TEST_DIRNAME" -p test_table_digests.py
  [ "$status" -eq 0 ]
}

@test "clean value: the Python derivation matches the Rust get_clean_value oracle" {
  run python3 -m unittest discover -s "$BATS_TEST_DIRNAME" -p test_clean_value.py
  [ "$status" -eq 0 ]
}

@test "run.sh and synthetic.py run: the table baseline is taken before the container starts and checked at every stage" {
  snapshot=$(grep -nF 'table_digests.py" snapshot' "$ADOPTION/run.sh" | head -n 1 | cut -d: -f1)
  start=$(grep -nF 'docker run -d' "$ADOPTION/run.sh" | head -n 1 | cut -d: -f1)
  [ -n "$snapshot" ] && [ "$snapshot" -lt "$start" ]
  grep -qF 'table_digests.py" check' "$ADOPTION/run.sh"
  # metadata_check runs after adoption, after restart and after scan
  [ "$(grep -c '^  metadata_check "after' "$ADOPTION/run.sh")" -eq 3 ]
  grep -qF 'table_changed = table_digests.compare(' "$ADOPTION/preservation.py"
  grep -qF 'changed += record("table digests", table_changed)' "$ADOPTION/preservation.py"
  grep -qF 'tables_before, table_digests.snapshot(db, tables_before, reauthenticated=reauth))' "$ADOPTION/preservation.py"
}

@test "messy rows: each shape's seeder and end-state reader, and the comparison with Jellyfin 12.2's record" {
  run python3 -m unittest discover -s "$BATS_TEST_DIRNAME" -p test_messy.py
  [ "$status" -eq 0 ]
}

# A messy fixture copy holding every shape, seeded by messy.py into a scratch database, with the
# manifest and an oracle record saying Jellyfin 12.2 left each shape as the seeders read it.
messy_fixture() { # messy_fixture <yes|no>: every shape seeded and recorded as Jellyfin 12.2 left it; "no" then loses the original of duplicate_paths
  mkdir -p messy
  python3 - "$ADOPTION" "$PWD/messy" "$1" <<'PY'
import json, sqlite3, sys
sys.path.insert(0, sys.argv[1]); sys.path.insert(0, sys.argv[1] + "/tests")
import messy
from test_messy import MANIFEST, populate

db = sys.argv[2] + "/jellyfin.db"
populate(db)
entry = messy.seed_messy(db, MANIFEST)
entry["lineage"] = "test"   # a lineage no divergence is ever written for, so the oracle record decides
state = messy.read_state(db, entry)
json.dump(entry, open(sys.argv[2] + "/messy.json", "w"))
json.dump({"shapes": {shape: {"outcome": "booted", "state": {shape: state[shape]}} for shape in entry["shapes"]}},
          open(sys.argv[2] + "/messy-oracle.json", "w"))
if sys.argv[3] == "no":
    with sqlite3.connect(db) as conn:
        conn.execute("DELETE FROM BaseItems WHERE lower(replace(Id, '-', '')) = ?", (entry["shapes"]["duplicate_paths"]["original"],))
PY
}

@test "check_messy: a shape that ends as Jellyfin 12.2 left it says nothing" {
  messy_fixture yes
  run adoption_check_messy messy/jellyfin.db messy
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "check_messy: a shape that ends differently names the shape and both states" {
  messy_fixture no
  run adoption_check_messy messy/jellyfin.db messy
  [[ "$output" == "messy: duplicate_paths: "*"expected"*"Jellyfin 12.2"* ]]
}

@test "check_messy: a checker that cannot run is a reason, never a pass" {
  messy_fixture yes
  rm messy/messy-oracle.json
  run adoption_check_messy messy/jellyfin.db messy
  [[ "$output" == "messy: the checker failed: "* ]]
}

@test "run.sh: --only a clean fixture never reaches the messy rows, and a messy fixture that is not built is skipped and named" {
  fake_docker; mkdir -p fixtures/oracle fixtures/media; smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0
  [ "${#lines[@]}" -eq 1 ]
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-messy
  [ "$status" -eq 0 ]
  [[ "${lines[0]}" == SKIP*12.2.0*jellyfin-12.2-messy*"fixture missing" ]]
}

# A database holding every table table_digests.py reads, with nothing in it.
empty_adoption_db() { # empty_adoption_db <file>
  local t
  sqlite3 "$1" 'CREATE TABLE Users (Id TEXT PRIMARY KEY); CREATE TABLE BaseItems (Id TEXT PRIMARY KEY, Path TEXT, OwnerId TEXT);'
  for t in MediaStreamInfos AttachmentStreamInfos Chapters TrickplayInfos MediaSegments KeyframeData ItemValues \
           ItemValuesMap Peoples PeopleBaseItemMap DisplayPreferences ItemDisplayPreferences \
           CustomItemDisplayPreferences Devices DeviceOptions ApiKeys ActivityLogs; do
    sqlite3 "$1" "CREATE TABLE \"$t\" (Id INTEGER PRIMARY KEY, UserId TEXT, ItemId TEXT, DeviceId TEXT);"
  done
}

@test "check_tables: nothing lost says nothing; a lost row is named; an unreadable baseline is a reason" {
  empty_adoption_db t.db
  sqlite3 t.db "INSERT INTO ApiKeys (Id) VALUES (1)"
  python3 "$ADOPTION/table_digests.py" snapshot t.db base.json
  run adoption_check_tables t.db base.json
  [ "$status" -eq 0 ]
  [ -z "$output" ]
  sqlite3 t.db "DELETE FROM ApiKeys"
  run adoption_check_tables t.db base.json
  [[ "$output" == "tables: 1 difference(s), first: ApiKeys row 1 is gone" ]]
  run adoption_check_tables t.db missing.json
  [[ "$output" == "tables: "*"difference(s), first: table digest check could not read"* ]]
}

@test "run.sh: a messy fixture whose container exits fails at once, naming it, instead of waiting out the deadlines" {
  mkdir -p bin fixtures/oracle fixtures/media fixtures/jellyfin-12.2-messy/data
  smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  empty_adoption_db fixtures/jellyfin-12.2-messy/data/jellyfin.db
  printf '{"lineage": "12.x", "shapes": {}}' > fixtures/jellyfin-12.2-messy/messy.json
  printf '{"shapes": {}}' > fixtures/jellyfin-12.2-messy/messy-oracle.json
  # image inspect succeeds; every container "has exited" (State.Running false); nothing else is asked
  cat > bin/docker <<'EOS'
#!/usr/bin/env bash
case "$1" in
  image|run|restart|rm|logs) exit 0;;
  inspect) echo false; exit 0;;
esac
echo "unexpected docker $*" >&2; exit 99
EOS
  chmod +x bin/docker; bin_first
  start=$SECONDS
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-messy
  [ "$status" -eq 1 ]
  [[ "$output" == FAIL*12.2.0*jellyfin-12.2-messy*"never became ready"*"second boot never ready"* ]]
  [[ "$output" == *"messy: "* ]]   # the shape checker ran and said why it could not pass
  [ $((SECONDS - start)) -lt 60 ]
  [[ "$output" != *"unexpected docker"* ]]
  # the report: every check's verdict was recorded, and the page says what failed and why
  work=fixtures/work/run
  [ "$(jq -s '[.[] | select(.event == "row")] | .[0] | [.fixture, .kind, .expected, .verdict] | join(" ")' -r "$work/results.jsonl")" = "jellyfin-12.2-messy messy 12.2.0 FAIL" ]
  [ "$(jq -s '[.[] | select(.event == "check" and .verdict == "FAIL")] | length' "$work/results.jsonl")" -ge 2 ]
  grep -q "jellyfin-12.2-messy" "$work/report.html"
  grep -q "never became ready" "$work/report.html"
  grep -q "messy shapes end as Jellyfin 12.2 left them" "$work/report.html"
  grep -q ">FAIL<" "$work/report.html"
}

@test "run.sh: a skipped fixture is a SKIP row in the report, which is written even when nothing ran" {
  fake_docker; mkdir -p fixtures/oracle fixtures/media; smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0
  [ "$status" -eq 0 ]
  [ "$(jq -r 'select(.event == "row") | [.fixture, .verdict, .detail] | join("|")' fixtures/work/run/results.jsonl)" = "jellyfin-12.0|SKIP|fixture missing" ]
  grep -q "NOTHING RAN" fixtures/work/run/report.html
}

# A messy 12.x row whose container "is running" and never answers, so the row waits for it, run in a
# session of its own; started_waiting_row sets $leader once the row has begun. The fake docker records its
# session, the run's worker's (run.sh's own work runs there, gl-920), in docker.sid.
started_waiting_row() {
  mkdir -p bin fixtures/oracle fixtures/media fixtures/jellyfin-12.2-messy/data
  smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  empty_adoption_db fixtures/jellyfin-12.2-messy/data/jellyfin.db
  printf '{"lineage": "12.x", "shapes": {}}' > fixtures/jellyfin-12.2-messy/messy.json
  printf '{"shapes": {}}' > fixtures/jellyfin-12.2-messy/messy-oracle.json
  cat > bin/docker <<'EOS'
#!/usr/bin/env bash
read -r _ _ _ _ _ sid _ < /proc/$$/stat; echo "$sid" > "$PWD/docker.sid.$$"; mv "$PWD/docker.sid.$$" "$PWD/docker.sid"
case "$1" in
  image|run|restart|rm|logs) exit 0;;
  inspect) echo true; exit 0;;
esac
echo "unexpected docker $*" >&2; exit 99
EOS
  chmod +x bin/docker; bin_first
  setsid "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-messy &
  leader=$!
  for _ in $(seq 1 40); do grep -q '"begin"' fixtures/work/run/results.jsonl 2>/dev/null && break; sleep 0.25; done
  grep -q '"begin"' fixtures/work/run/results.jsonl
}

# session_pids <sid>: the processes still in that session (from /proc: busybox has no pgrep -s).
session_pids() {
  local stat pid s
  for stat in /proc/[0-9]*/stat; do
    read -r pid _ _ _ _ s _ 2>/dev/null < "$stat" || continue
    [ "$s" != "$1" ] || echo "$pid"
  done
}

# stopped_within <seconds>: the run ends in time, else it is killed and the test fails (a run that ignores
# its TERM would otherwise wait out two 600 s readiness deadlines, as gl-920's did on CI).
stopped_within() {
  local _
  for _ in $(seq 1 $(($1 * 4))); do kill -0 "$leader" 2>/dev/null || break; sleep 0.25; done
  if kill -0 "$leader" 2>/dev/null; then
    kill -KILL -- "-$leader" 2>/dev/null || true
    [ ! -s docker.sid ] || session_pids "$(cat docker.sid)" | xargs -r kill -KILL 2>/dev/null || true
    echo "run.sh was still running $1 s after its TERM" >&2
    return 1
  fi
  status=0; wait "$leader" || status=$?
}

@test "run.sh: a run that is terminated still leaves a report, listing the fixture it was on as unfinished" {
  started_waiting_row
  kill -TERM -- "-$leader"
  stopped_within 30
  [ "$status" -eq 143 ]
  grep -q "INCOMPLETE" fixtures/work/run/report.html
  grep -q "jellyfin-12.2-messy" fixtures/work/run/report.html
}

@test "run.sh: a TERM to run.sh alone stops everything the run started" {
  started_waiting_row
  sid=$(cat docker.sid)   # the worker's: its own docker calls come before the row begins
  [ -n "$sid" ]
  [ "$sid" != "$leader" ]   # the work runs in a session of its own
  kill -TERM "$leader"
  stopped_within 30
  [ "$status" -eq 143 ]
  grep -q "INCOMPLETE" fixtures/work/run/report.html
  # what the TERM killed may take a moment to be reaped
  for _ in $(seq 1 20); do [ -n "$(session_pids "$sid")" ] || break; sleep 0.1; done
  [ -z "$(session_pids "$sid")" ]
}

@test "adoption report: recording, verdicts, the page and the viewer's listing" {
  run python3 -m unittest discover -s "$BATS_TEST_DIRNAME" -p test_adoption_report.py
  [ "$status" -eq 0 ]
}

# A messy 12.x row for run.sh with a docker that logs its `run` arguments to $DOCKER_LOG and
# whose server never answers: enough to reach the mount and the table baseline.
logging_messy_row() { # logging_messy_row <source name or empty>
  mkdir -p bin fixtures/oracle fixtures/media fixtures/jellyfin-12.2-messy/data
  smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  empty_adoption_db fixtures/jellyfin-12.2-messy/data/jellyfin.db
  printf '{"lineage": "12.x", "source": "%s", "shapes": {}}' "$1" > fixtures/jellyfin-12.2-messy/messy.json
  printf '{"shapes": {}}' > fixtures/jellyfin-12.2-messy/messy-oracle.json
  cat > bin/docker <<'EOS'
#!/usr/bin/env bash
case "$1" in
  run) echo "$*" >> "$DOCKER_LOG"; exit 0;;
  image|restart|rm|logs) exit 0;;
  inspect) echo false; exit 0;;
esac
echo "unexpected docker $*" >&2; exit 99
EOS
  chmod +x bin/docker; bin_first
  export DOCKER_LOG="$TMP/docker.log"
}

@test "run.sh: the messy rows mount media-messy when it is built, the shared media otherwise" {
  logging_messy_row jellyfin-12.2-from-121
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-messy
  grep -q "$TMP/fixtures/media:/media:ro" "$DOCKER_LOG"          # a fixture directory built before media-messy existed
  [ "$(grep -c "media-messy" "$DOCKER_LOG")" = 0 ]
  mkdir fixtures/media-messy; : > "$DOCKER_LOG"
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-messy
  grep -q "$TMP/fixtures/media-messy:/media:ro" "$DOCKER_LOG"
  [ "$(grep -c "$TMP/fixtures/media:/media:ro" "$DOCKER_LOG")" = 0 ]
}

@test "run.sh: the messy rows' table baseline is the clean source fixture's, else the copy's" {
  logging_messy_row jellyfin-12.2-from-121
  mkdir -p fixtures/jellyfin-12.2-from-121/data
  empty_adoption_db fixtures/jellyfin-12.2-from-121/data/jellyfin.db
  sqlite3 fixtures/jellyfin-12.2-from-121/data/jellyfin.db "INSERT INTO ApiKeys (Id) VALUES (1)"
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-messy
  baseline=fixtures/work/run/jellyfin-12.2-messy.tables.json
  [ "$(jq '.tables.ApiKeys.rows | length' "$baseline")" = 1 ]          # the row only the clean source holds
  [ ! -e fixtures/jellyfin-12.2-from-121/data/jellyfin.db-wal ]        # the fixture itself was not opened
  printf '{"lineage": "12.x", "shapes": {}}' > fixtures/jellyfin-12.2-messy/messy.json   # no source named
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-messy
  [ "$(jq '.tables.ApiKeys.rows | length' "$baseline")" = 0 ]
  rm -r fixtures/jellyfin-12.2-from-121
  printf '{"lineage": "12.x", "source": "jellyfin-12.2-from-121", "shapes": {}}' > fixtures/jellyfin-12.2-messy/messy.json   # source gone
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-messy
  [ "$(jq '.tables.ApiKeys.rows | length' "$baseline")" = 0 ]
}

@test "build-fixtures.sh: the messy copies are built last, after the schema diff" {
  [ "$(grep -n 'messy.py" build' "$ADOPTION/build-fixtures.sh" | cut -d: -f1)" -gt "$(grep -n 'jellyfin-12.2-schema.sql: ' "$ADOPTION/build-fixtures.sh" | cut -d: -f1)" ]
}

@test "scale fixture: cloning, survival check, disk requirement, sampling and recording" {
  run python3 -m unittest discover -s "$BATS_TEST_DIRNAME" -p test_scale.py
  [ "$status" -eq 0 ]
}

# A scale fixture: the table shapes table_digests.py reads, and one cloned item.
scale_fixture() { # scale_fixture <directory>
  mkdir -p "$1/data"
  empty_adoption_db "$1/data/jellyfin.db"
  sqlite3 "$1/data/jellyfin.db" "INSERT INTO BaseItems VALUES ('A', '/media/movies/scale-00001/a.mkv', NULL)"
}

@test "check_scale: nothing lost says nothing; a lost clone is named; an unreadable baseline is a reason" {
  scale_fixture f
  python3 "$ADOPTION/scale.py" counts f/data/jellyfin.db base.json
  run adoption_check_scale f/data/jellyfin.db base.json
  [ "$status" -eq 0 ]
  [ -z "$output" ]
  sqlite3 f/data/jellyfin.db "DELETE FROM BaseItems"
  run adoption_check_scale f/data/jellyfin.db base.json
  [[ "$output" == "scale: BaseItems: 1 rows before, 0 after"* ]]
  run adoption_check_scale f/data/jellyfin.db missing.json
  [[ "$output" == "scale: the checker failed: scale: "* ]]
}

@test "run.sh: the scale fixture is skipped, named, when it is not built, and --only a clean fixture never reaches it" {
  fake_docker; mkdir -p fixtures/oracle fixtures/media; smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-10.11.11-scale
  [ "$status" -eq 0 ]
  [[ "${lines[0]}" == SKIP*10.11.11*jellyfin-10.11.11-scale*"fixture missing" ]]
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0
  [ "${#lines[@]}" -eq 1 ]
}

@test "run.sh: the scale row refuses, naming the disk it needs, before it copies or starts anything" {
  fake_docker; mkdir -p fixtures/oracle fixtures/media; smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  scale_fixture fixtures/jellyfin-10.11.11-scale
  truncate -s 8T fixtures/jellyfin-10.11.11-scale/data/jellyfin.db   # a sparse database no disk has six times over
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-10.11.11-scale
  [ "$status" -eq 1 ]
  [[ "$output" == FAIL*10.11.11*jellyfin-10.11.11-scale*"needs "*" free in "*"6x the 8192.0 GiB database"* ]]
  [ ! -e fixtures/work/run/jellyfin-10.11.11-scale ]
  [[ "$output" != *"unexpected docker"* ]]
}

@test "run.sh: a scale fixture whose container exits fails at once, saying which numbers were not measured" {
  mkdir -p bin fixtures/oracle fixtures/media
  smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  scale_fixture fixtures/jellyfin-10.11.11-scale
  cat > bin/docker <<'EOS'
#!/usr/bin/env bash
case "$1" in
  image|run|restart|rm|logs) exit 0;;
  inspect) echo false; exit 0;;
esac
echo "unexpected docker $*" >&2; exit 99
EOS
  chmod +x bin/docker; bin_first
  start=$SECONDS
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-10.11.11-scale
  [ "$status" -eq 1 ]
  [[ "$output" == FAIL*10.11.11*jellyfin-10.11.11-scale*"never became ready"*"second boot never ready"* ]]
  [[ "$output" == *"not measured: "* ]]     # the numbers that could not be taken are named, never defaulted
  [ $((SECONDS - start)) -lt 60 ]
  [[ "$output" != *"unexpected docker"* ]]
}

@test "run.sh: the scale row gives up on a server that never answers after ADOPTION_SCALE_TIMEOUT, on both boots" {
  mkdir -p bin fixtures/oracle fixtures/media
  smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  scale_fixture fixtures/jellyfin-10.11.11-scale
  # the container stays up and never answers; without the limit the first wait alone is 30 minutes
  cat > bin/docker <<'EOS'
#!/usr/bin/env bash
case "$1" in
  image|run|restart|rm|logs) exit 0;;
  inspect) echo true; exit 0;;
esac
echo "unexpected docker $*" >&2; exit 99
EOS
  chmod +x bin/docker; bin_first
  start=$SECONDS
  ADOPTION_SCALE_TIMEOUT=1 run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-10.11.11-scale
  [ "$status" -eq 1 ]
  [[ "$output" == FAIL*jellyfin-10.11.11-scale*"never became ready"*"second boot never ready"* ]]
  [ $((SECONDS - start)) -lt 60 ]
}

@test "build-fixtures.sh: the scale copy is built after the messy copies, the last step" {
  [ "$(grep -n 'scale.py" build' "$ADOPTION/build-fixtures.sh" | cut -d: -f1)" -gt "$(grep -n 'messy.py" build' "$ADOPTION/build-fixtures.sh" | cut -d: -f1)" ]
}

@test "populated upgrade: user state, boot checks and the runner's refusals" {
  run python3 -m unittest discover -s "$BATS_TEST_DIRNAME" -p test_upgrade.py
  [ "$status" -eq 0 ]
}

@test "synthetic.py upgrade: refuses without a start release, then without the fixture, before docker runs" {
  fake_docker; mkdir fixtures
  run python3 "$ADOPTION/synthetic.py" upgrade --fixtures fixtures --image any
  [ "$status" -eq 1 ]
  [[ "$output" == *"at least one --from image"* ]]
  run python3 "$ADOPTION/synthetic.py" upgrade --fixtures fixtures --image any --from ghcr.io/x/ferrofin:v1.3.2
  [ "$status" -eq 1 ]
  [[ "$output" == *"jellyfin-10.11.8 is not built"* ]]
  [[ "$output" != *"unexpected docker"* ]]
}
