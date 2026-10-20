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
  unset FERROFIN_ADOPTION_FIXTURES IMAGE ADOPTION_USER ADOPTION_WORK_DIR ADOPTION_SCAN_TIMEOUT ADOPTION_ORACLE ORACLE_VERSION
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
  [ "$(grep -E '^  "[0-9.]+\|jellyfin-' "$ADOPTION/run.sh" | grep -vc -- '-messy|')" -eq 11 ]
  # and the two messy copies, on ports of their own (the uniqueness check above covers them too)
  [ "$(grep -cE '^  "[0-9.]+\|jellyfin-[^|]*-messy\|' "$ADOPTION/run.sh")" -eq 2 ]
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

@test "adoption_require_local: network filesystems are refused, local ones (FUSE included) accepted" {
  for fs in nfs nfs4 cifs smb3 9p ceph afs glusterfs fuse.sshfs fuse.rclone autofs,nfs; do
    fake_findmnt "$fs"
    run bash -c '. "$1/lib.sh"; PATH="$2:$PATH" adoption_require_local "$3"' _ "$ADOPTION" "$TMP/bin" "$TMP"
    [ "$status" -eq 1 ]
    [[ "$output" == *"network filesystem (${fs##*,})"* ]]
  done
  for fs in ext4 btrfs tmpfs fuseblk fuse.mergerfs; do
    fake_findmnt "$fs"
    run bash -c '. "$1/lib.sh"; PATH="$2:$PATH" adoption_require_local "$3"' _ "$ADOPTION" "$TMP/bin" "$TMP"
    [ "$status" -eq 0 ]
  done
  # a directory not created yet is judged by its nearest existing parent
  run adoption_require_local "$TMP/not/yet/made"
  [ "$status" -eq 0 ]
  # a filesystem findmnt cannot name is refused, never assumed local
  printf '#!/usr/bin/env bash\nexit 1\n' > bin/findmnt
  run bash -c '. "$1/lib.sh"; PATH="$2:$PATH" adoption_require_local "$3"' _ "$ADOPTION" "$TMP/bin" "$TMP"
  [ "$status" -eq 1 ]
  [[ "$output" == *"cannot tell its filesystem"* ]]
}

@test "run.sh: the only media mounted is the fixtures' own media, read-only" {
  run grep -n 'MEDIA=' "$ADOPTION/run.sh"
  [ "${#lines[@]}" -eq 1 ]
  # shellcheck disable=SC2016 # the script's literal source line, not an expansion
  [[ "$output" == *'MEDIA=(-v "$FIXTURES/media:/media:ro")' ]]
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
    echo '{"shapes": {"deleted_user": {}, "dangling_owner_extra": {}, "duplicate_paths": {}}}' > "fixtures/$f/messy.json"
  done
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
  grep -qF 'changed += table_digests.compare(' "$ADOPTION/preservation.py"
  grep -qF 'tables_before, table_digests.snapshot(db, tables_before, reauthenticated=reauth))' "$ADOPTION/preservation.py"
}

@test "messy rows: each shape's seeder and end-state reader, and the comparison with Jellyfin 12.2's record" {
  run python3 -m unittest discover -s "$BATS_TEST_DIRNAME" -p test_messy.py
  [ "$status" -eq 0 ]
}

# A messy fixture copy holding every shape (the deleted user's tables empty, the dangling extra
# absent, one duplicate-path row), with the manifest and the oracle record messy.py reads: the
# record says Jellyfin 12.2 left the same, except for the duplicate path it records as given.
messy_fixture() { # messy_fixture <original-kept> <oracle original> <oracle duplicate>
  local t zeros=
  mkdir -p messy
  sqlite3 messy/jellyfin.db "CREATE TABLE BaseItems (Id TEXT PRIMARY KEY, Path TEXT, OwnerId TEXT);
    INSERT INTO BaseItems (Id, Path) VALUES ('$(printf 'a%.0s' {1..32})', '/p');"
  for t in UserData Permissions Preferences Devices DisplayPreferences; do
    sqlite3 messy/jellyfin.db "CREATE TABLE \"$t\" (UserId TEXT);"
    zeros="$zeros${zeros:+, }\"$t\": 0"
  done
  [ "$1" = yes ] || sqlite3 messy/jellyfin.db "DELETE FROM BaseItems"
  # a lineage no divergence is ever written for, so the oracle record decides
  printf '{"lineage": "test", "shapes": {"deleted_user": {"user": "%s"}, "dangling_owner_extra": {"extra": "%s", "owner": "%s"},
    "duplicate_paths": {"path": "/p", "original": "%s", "duplicate": "%s"}}}' \
    "$(printf 'c%.0s' {1..32})" "$(printf 'd%.0s' {1..32})" "$(printf 'e%.0s' {1..32})" \
    "$(printf 'a%.0s' {1..32})" "$(printf 'b%.0s' {1..32})" > messy/messy.json
  printf '{"shapes": {"deleted_user": {"outcome": "booted", "state": {"deleted_user": {%s}}},
    "dangling_owner_extra": {"outcome": "booted", "state": {"dangling_owner_extra": {"exists": false, "owner": null}}},
    "duplicate_paths": {"outcome": "booted", "state": {"duplicate_paths": {"original": %s, "duplicate": %s}}}}}' \
    "$zeros" "$2" "$3" > messy/messy-oracle.json
}

@test "check_messy: a shape that ends as Jellyfin 12.2 left it says nothing" {
  messy_fixture yes true false
  run adoption_check_messy messy/jellyfin.db messy
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "check_messy: a shape that ends differently names the shape and both states" {
  messy_fixture no true false
  run adoption_check_messy messy/jellyfin.db messy
  [[ "$output" == "messy: duplicate_paths: "*"expected"*"Jellyfin 12.2"* ]]
}

@test "check_messy: a checker that cannot run is a reason, never a pass" {
  messy_fixture yes true false
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
  chmod +x bin/docker; PATH="$TMP/bin:$PATH"
  start=$SECONDS
  run "$ADOPTION/run.sh" --fixtures fixtures --image any --only jellyfin-12.2-messy
  [ "$status" -eq 1 ]
  [[ "$output" == FAIL*12.2.0*jellyfin-12.2-messy*"never became ready"*"second boot never ready"* ]]
  [[ "$output" == *"messy: "* ]]   # the shape checker ran and said why it could not pass
  [ $((SECONDS - start)) -lt 60 ]
  [[ "$output" != *"unexpected docker"* ]]
}

@test "build-fixtures.sh: the messy copies are built last, after the schema diff" {
  [ "$(grep -n 'messy.py" build' "$ADOPTION/build-fixtures.sh" | cut -d: -f1)" -gt "$(grep -n 'jellyfin-12.2-schema.sql: ' "$ADOPTION/build-fixtures.sh" | cut -d: -f1)" ]
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
