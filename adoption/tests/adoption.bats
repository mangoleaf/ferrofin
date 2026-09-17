#!/usr/bin/env bats
# Tests for the adoption harness (adoption/lib.sh, run.sh, smoke.sh, build-fixtures.sh).
# Nothing here needs docker, a Jellyfin database or the network: the checks in lib.sh are
# driven with canned logs and smoke outputs, and the SQLite checks with throwaway files.

setup() {
  ADOPTION="$BATS_TEST_DIRNAME/.."
  # shellcheck source=adoption/lib.sh
  . "$ADOPTION/lib.sh"
  TMP="$(mktemp -d)"
  cd "$TMP" || exit 1
}

teardown() {
  rm -rf "$TMP"
}

# A smoke output in the real format: "status  metric  path".
smoke() { # smoke <file> <episode-count> [playlists-view-listed]
  cat > "$1" <<EOS
200  12.1.0 Jellyfin Server                        /System/Info/Public
200  mango admin=true                              /Users/Me
200  Movies,${3:+Playlists,}TV                     /Users/9eccc7ec621f47cfadcb3cd4a0c29227/Views
200  $2                                            /Items?UserId=9eccc7ec621f47cfadcb3cd4a0c29227&Recursive=true&IncludeItemTypes=Episode&Limit=0
200  26 running=                                   /ScheduledTasks
200  (bytes=$RANDOM)                               /Items/acdfe2d0ff1b6e2d9d25b396c3665a05/Images/Primary?maxWidth=200
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
  smoke a.txt 6451; smoke b.txt 6451
  run diff <(adoption_normalise a.txt) <(adoption_normalise b.txt)
  [ "$status" -eq 0 ]
  run adoption_normalise a.txt
  [[ "$output" == *"/Users/<id>/Views"* ]]
  [[ "$output" == *"(bytes)"* ]]
  [[ "$output" != *"/ScheduledTasks"* ]]
  [[ "$output" != *"/System/Info"* ]]
}

@test "normalise: a real difference survives" {
  smoke a.txt 6451; smoke b.txt 6307
  run diff <(adoption_normalise a.txt) <(adoption_normalise b.txt)
  [ "$status" -eq 1 ]
}

# --- boot log ----------------------------------------------------------------

@test "check_boot_log: a clean boot of the expected generation says nothing" {
  boot_log boot.log 12.1.0
  run adoption_check_boot_log boot.log 12.1.0
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
  smoke oracle.txt 6451 yes; smoke same.txt 6451 yes; smoke fewer.txt 6307 yes
  run adoption_compare_smoke oracle.txt same.txt
  [ -z "$output" ]
  run adoption_compare_smoke oracle.txt fewer.txt
  [ "$output" = "smoke differs from Jellyfin 12.1 (diff oracle.txt fewer.txt)" ]
}

@test "compare_smoke: a view that is listed by one server and not the other fails" {
  smoke oracle.txt 6451; smoke views.txt 6451 yes
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
  [ -z "$output" ]
  echo '{"fields":{"message":"imported playlist/collection/version membership from Data JSON","rows":2850}}' >> second.log
  run adoption_second_boot_repairs second.log
  [[ "$output" == *"imported playlist/collection/version membership"* ]]
}

@test "second_boot_repairs: promotions are work even when repaired is zero" {
  echo '{"fields": {"message": "repaired alternate-version primaries from LinkedChildren", "repaired": 0, "promoted": 1}}' > second.log
  run adoption_second_boot_repairs second.log
  [[ "$output" == *'repaired alternate-version primaries'* ]]
}

@test "second_boot_repairs: all-zero counters are ignored regardless of their order" {
  echo '{"fields": {"promoted": 0, "message": "repaired alternate-version primaries from LinkedChildren", "repaired": 0}}' > second.log
  run adoption_second_boot_repairs second.log
  [ -z "$output" ]
}

@test "second_boot_repairs: a repair without counters is still reported" {
  echo '{"fields":{"message":"repaired OwnerId relationships"}}' > second.log
  run adoption_second_boot_repairs second.log
  [[ "$output" == *'repaired OwnerId relationships'* ]]
}

# --- credentials ----------------------------------------------------------------------

seed_users_db() {
  sqlite3 users.db <<'EOS'
CREATE TABLE Users (Id TEXT PRIMARY KEY, Username TEXT);
CREATE TABLE Permissions (UserId TEXT, Kind INTEGER, Value INTEGER);
CREATE TABLE Devices (UserId TEXT, AccessToken TEXT, DeviceId TEXT, AppName TEXT, DateLastActivity TEXT);
INSERT INTO Users VALUES ('AAAA-1', 'mango'), ('BBBB-2', 'shared');
INSERT INTO Permissions VALUES ('AAAA-1', 0, 1), ('BBBB-2', 0, 0);
INSERT INTO Devices VALUES ('AAAA-1', 'tok-mango', 'dev-mango', 'Jellyfin Web', '2026-01-01');
INSERT INTO Devices VALUES ('AAAA-1', 'tok-old',   'dev-old',   'Jellyfin Web', '2025-01-01');
INSERT INTO Devices VALUES ('BBBB-2', 'tok-shared','dev-shared','Jellyfin Web', '2026-09-01');
INSERT INTO Devices VALUES ('BBBB-2', 'tok-tv',    'dev-tv',    'Wholphin',     '2026-09-15');
EOS
}

@test "smoke_credentials: defaults to the administrator's newest web session, not the most recent user" {
  seed_users_db
  run adoption_smoke_credentials users.db
  [ "$status" -eq 0 ]
  [ "$output" = "mango tok-mango dev-mango aaaa1" ]
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

fake_docker() { # a docker on PATH that has every image and no containers
  mkdir -p bin; cat > bin/docker <<'EOS'
#!/usr/bin/env bash
case "$1 $2" in "image inspect") exit 0;; esac
echo "unexpected docker $*" >&2; exit 99
EOS
  chmod +x bin/docker; PATH="$TMP/bin:$PATH"
}

@test "run.sh: refuses to run without a fixtures directory" {
  run "$ADOPTION/run.sh"
  [ "$status" -eq 2 ]
  [[ "$output" == *"--fixtures DIR"* ]]
}

@test "run.sh: refuses to run without the oracle" {
  fake_docker; mkdir fixtures
  run "$ADOPTION/run.sh" --fixtures fixtures --image any
  [ "$status" -eq 2 ]
  [[ "$output" == *"oracle/smoke-jellyfin-12.1.txt missing"* ]]
}

@test "run.sh: a missing fixture is skipped, named, and does not fail the run" {
  fake_docker; mkdir -p fixtures/oracle; smoke fixtures/oracle/smoke-jellyfin-12.1.txt 6451
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0
  [ "$status" -eq 0 ]
  [ "${#lines[@]}" -eq 1 ]
  [[ "${lines[0]}" == SKIP*12.0.0*jellyfin-12.0*"fixture missing" ]]
}

@test "run.sh: every supported release has a row" {
  for f in jellyfin-10.11.8 jellyfin-10.11.9 jellyfin-10.11.10 jellyfin-10.11.11 jellyfin-12.0 jellyfin-12.1-from-10 jellyfin-12.1-from-12; do
    grep -q "|$f|" "$ADOPTION/run.sh"
  done
}

# --- build-fixtures.sh preconditions ----------------------------------------------------

@test "build-fixtures.sh: refuses without the 10.11.8 snapshot" {
  mkdir fixtures
  run "$ADOPTION/build-fixtures.sh" --fixtures fixtures
  [ "$status" -eq 2 ]
  [[ "$output" == *"no "*"jellyfin-10.11.8/data/jellyfin.db"* ]]
}

@test "build-fixtures.sh: refuses a snapshot that is not a 10.11.8 database" {
  mkdir -p fixtures/jellyfin-10.11.8/data
  sqlite3 fixtures/jellyfin-10.11.8/data/jellyfin.db 'CREATE TABLE __EFMigrationsHistory (MigrationId TEXT, ProductVersion TEXT); INSERT INTO __EFMigrationsHistory VALUES ("a","1"),("b","1"),("c","1");'
  run "$ADOPTION/build-fixtures.sh" --fixtures fixtures
  [ "$status" -eq 2 ]
  [[ "$output" == *"has 3 EF migration ids, a 10.11.8 database has 68"* ]]
}
