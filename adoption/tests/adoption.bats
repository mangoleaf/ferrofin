#!/usr/bin/env bats
# Tests for the adoption harness (adoption/lib.sh, run.sh, smoke.sh, build-fixtures.sh).
# Nothing here needs docker, a Jellyfin database or external services: tests use canned
# logs and smoke outputs, throwaway SQLite files and a local HTTP server.

setup() {
  ADOPTION="$BATS_TEST_DIRNAME/.."
  # shellcheck source=adoption/lib.sh
  . "$ADOPTION/lib.sh"
  TMP="$BATS_TEST_TMPDIR/fixtures"
  mkdir -p "$TMP"
  cd "$TMP" || exit 1
  unset FERROFIN_ADOPTION_FIXTURES IMAGE ADOPTION_USER ADOPTION_WORK_DIR ADOPTION_SCAN_TIMEOUT ADOPTION_ORACLE ADOPTION_MEDIA_MOUNTS ORACLE_VERSION
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
  [[ "$output" == *"oracle/smoke-jellyfin-12.2.txt missing"* ]]
}

@test "run.sh: a missing fixture is skipped, named, and does not fail the run" {
  fake_docker; mkdir -p fixtures/oracle; smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
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
  # ten paths, each on its own port
  run bash -c "grep -oE '^  \"[0-9.]+\\|jellyfin-[^|]+\\|[0-9]+\"' \"\$1\" | cut -d'|' -f3 | sort | uniq -d" _ "$ADOPTION/run.sh"
  [ -z "$output" ]
  [ "$(grep -cE '^  "[0-9.]+\|jellyfin-' "$ADOPTION/run.sh")" -eq 10 ]
}

@test "run.sh: only a 10.11 source compares /Persons, and the whole smoke again, after the scan" {
  grep -qF "case \$expected in 10.11.*) after_scan='/Persons\\?';; *) after_scan=;; esac" "$ADOPTION/run.sh"
  grep -qF "adoption_compare_smoke \"\$ORACLE\" \"\$dst.smoke.txt\" \"\$after_scan\")" "$ADOPTION/run.sh"
  grep -qF "adoption_compare_smoke \"\$ORACLE\" \"\$dst.smoke-scan.txt\" | sed 's/^/after scan: /')" "$ADOPTION/run.sh"
}

@test "run.sh: the second-boot log starts at the restarted container's own start, and must hold a server start" {
  # a zone-less `date -u` --since is read as local time: empty west of UTC, the first boot east of it
  run grep -cF "logs --since \"\$(date" "$ADOPTION/run.sh"
  [ "$output" = 0 ]
  grep -qF "docker logs --since \"\$(docker inspect -f '{{.State.StartedAt}}' \"\$name\")\" \"\$name\" > \"\$dst.boot2.log\" 2>&1" "$ADOPTION/run.sh"
  grep -qF "grep -q '\"ferrofin-server starting\"' \"\$dst.boot2.log\" || why+=(\"second boot log has no server start\")" "$ADOPTION/run.sh"
}

@test "run.sh: the oracle is Jellyfin 12.2 unless --oracle or ADOPTION_ORACLE picks 12.1" {
  fake_docker; mkdir -p fixtures/oracle; smoke fixtures/oracle/smoke-jellyfin-12.1.txt 120
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

@test "run.sh: a media-mounts file named in ADOPTION_MEDIA_MOUNTS must exist" {
  fake_docker; mkdir -p fixtures/oracle; smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  ADOPTION_MEDIA_MOUNTS=$TMP/nowhere.sh run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0
  [ "$status" -eq 2 ]
  [[ "$output" == *"no $TMP/nowhere.sh"* ]]
  printf 'MEDIA=(-v /x:/y:ro)\n' > mounts.sh
  ADOPTION_MEDIA_MOUNTS=$TMP/mounts.sh run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0
  [ "$status" -eq 0 ]
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
  sqlite3 fixtures/jellyfin-10.11.8/data/jellyfin.db "CREATE TABLE __EFMigrationsHistory (MigrationId TEXT, ProductVersion TEXT); INSERT INTO __EFMigrationsHistory VALUES ('a','1'),('b','1'),('c','1');"
  run "$ADOPTION/build-fixtures.sh" --fixtures fixtures
  [ "$status" -eq 2 ]
  [[ "$output" == *"has 3 EF migration ids, a 10.11.8 database has 68"* ]]
}

@test "build-fixtures.sh: each 12.2 fixture boots Jellyfin 12.2 on its contract parent" {
  grep -q ' jellyfin-12.2-from-10:jellyfin-10.11.8:[0-9]*:107 ' "$ADOPTION/build-fixtures.sh"
  grep -q ' jellyfin-12.2-from-12:jellyfin-12.0:[0-9]*:108 ' "$ADOPTION/build-fixtures.sh"
  grep -q ' jellyfin-12.2-from-121:jellyfin-12.1-from-12:[0-9]*:109;' "$ADOPTION/build-fixtures.sh"
  grep -q '^build_oracle 12.1 jellyfin-12.1-from-12 ' "$ADOPTION/build-fixtures.sh"
  grep -q '^build_oracle 12.2 jellyfin-12.2-from-121 ' "$ADOPTION/build-fixtures.sh"
}

@test "build-fixtures.sh: with every fixture built it only dumps the 12.2 schema and diffs the other routes" {
  mkdir -p bin; printf '#!/usr/bin/env bash\necho "unexpected docker $*" >&2; exit 99\n' > bin/docker; chmod +x bin/docker
  PATH="$TMP/bin:$PATH"
  mkdir -p fixtures/jellyfin-10.11.8/data fixtures/oracle
  sqlite3 fixtures/jellyfin-10.11.8/data/jellyfin.db "CREATE TABLE __EFMigrationsHistory (MigrationId TEXT, ProductVersion TEXT);
    WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 68) INSERT INTO __EFMigrationsHistory SELECT i, '9' FROM n;"
  for f in 10.11.9 10.11.10 10.11.11 12.0 12.1-from-10 12.1-from-12; do mkdir "fixtures/jellyfin-$f"; done
  for f in 12.2-from-10 12.2-from-12 12.2-from-121; do
    mkdir -p "fixtures/jellyfin-$f/data"
    sqlite3 "fixtures/jellyfin-$f/data/jellyfin.db" 'CREATE TABLE t(a); CREATE INDEX i ON t(a) WHERE a IS NOT NULL;
      INSERT INTO t VALUES (1); ANALYZE; CREATE TABLE s(id INTEGER PRIMARY KEY AUTOINCREMENT); INSERT INTO s DEFAULT VALUES;'
  done
  sqlite3 fixtures/jellyfin-12.2-from-12/data/jellyfin.db 'CREATE INDEX extra ON t(a);'
  smoke fixtures/oracle/smoke-jellyfin-12.1.txt 120; smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  run "$ADOPTION/build-fixtures.sh" --fixtures fixtures
  [ "$status" -eq 0 ]
  [[ "$output" == *"jellyfin-12.2-schema.sql: 3 CREATE statements"* ]]
  [[ "$output" == *"jellyfin-12.2-from-10: same schema"* ]]
  [[ "$output" == *"jellyfin-12.2-from-12: schema DIFFERS"* ]]
  [[ "$output" != *"unexpected docker"* ]]
  grep -q 'WHERE a IS NOT NULL' fixtures/jellyfin-12.2-schema.sql
  run grep -cE 'sqlite_stat1|sqlite_sequence' fixtures/jellyfin-12.2-schema.sql
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
  mkdir -p fixtures/oracle fixtures/jellyfin-12.0/data
  smoke fixtures/oracle/smoke-jellyfin-12.2.txt 120
  printf 'not a database\n' > fixtures/jellyfin-12.0/data/jellyfin.db
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.0
  [ "$status" -eq 1 ]
  [[ "$output" == *"metadata baseline: metadata check could not read"* ]]
  [[ "$output" != *"unexpected docker run"* ]]
}

@test "synthetic adoption checks detect missing scenarios, changed membership and redundant writes" {
  run python3 -m unittest discover -s "$ADOPTION/tests" -p test_preservation.py
  [ "$status" -eq 0 ]
}

@test "user accounts: avatars, login states and complete settings survive adoption" {
  run python3 -m unittest discover -s "$BATS_TEST_DIRNAME" -p test_user_accounts.py
  [ "$status" -eq 0 ]
}
