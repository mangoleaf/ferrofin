#!/usr/bin/env bash
# gates/adoption/build-fixtures.sh [--fixtures DIR]
#
# Builds every fixture gates/adoption/run.sh needs from synthetic data only — no real library, no
# media server, no network share. synthetic.py generates invented media under DIR/media and has
# each Jellyfin release build its own fixture through its API (see VERSIONS there):
#   jellyfin-10.11.8/                 Jellyfin 10.11.8 seeded from scratch
#   jellyfin-10.11.9/  10.11.10/  10.11.11/   that release booted and scanned on a copy of 10.11.8
#   jellyfin-12.0/  12.1-from-10/  12.1-from-12/  12.2-from-10/  12.2-from-12/  12.2-from-121/
#   jellyfin-12.3-from-122/  12.3-from-10/  12.4-from-123/  12.4-from-10/
#   jellyfin-12.2-fresh/  12.3-fresh/  12.4-fresh/   that release seeded from scratch (a first run, not upgraded)
# Then, next to them:
#   oracle/smoke-jellyfin-12.1.txt smoke.sh against Jellyfin 12.1 on a throwaway copy of 12.1-from-12
#   oracle/smoke-jellyfin-12.2.txt smoke.sh against Jellyfin 12.2 on a throwaway copy of 12.2-from-121
#   oracle/smoke-jellyfin-12.2-fresh.txt  smoke.sh against Jellyfin 12.2 on a copy of 12.2-fresh
#   oracle/smoke-jellyfin-12.4.txt smoke.sh against Jellyfin 12.4 on a throwaway copy of 12.4-from-123
#   oracle/smoke-jellyfin-12.4-fresh.txt  smoke.sh against Jellyfin 12.4 on a copy of 12.4-fresh
#   oracle/user.txt                the account every oracle (and run.sh) probes as
#   jellyfin-12.2-schema.sql       12.2-from-121's .schema without sqlite_stat*/sqlite_sequence,
#                                  rewritten every run; every other upgrade route of 12.2, 12.3 and
#                                  12.4 is diffed against it, and the 12.3 and 12.4 fresh installs
#                                  against 12.2's
#   jellyfin-12.3-schema.sql  jellyfin-12.4-schema.sql   the same dump of 12.3-from-122 and 12.4-from-123
#   jellyfin-10.11.11-messy/  jellyfin-12.4-messy/   copies of 10.11.11 and 12.4-from-123 plus rows of
#                                  shapes no API produces, each with Jellyfin 12.4's end state for it
#                                  (messy.py: messy.json, messy-oracle.json)
#   media-messy/                   a hard-linked copy of media/ plus a link per file a seeded item needs; only the
#                                  messy rows and their oracle mount it, so the clean rows' media is untouched
#   jellyfin-10.11.11-scale/       a copy of 10.11.11 whose media items are cloned by SQL up to 40,000
#                                  (scale.py: scale.json); no oracle, no scan
#   media-scale/                   a hard-linked copy of media/ plus a link at each clone's path; only the
#                                  scale row mounts it
# Existing fixtures and oracles are kept; synthetic.py refuses fixtures from an older recipe, so
# build into a new DIR after changing it. DIR must be on a local disk. An interrupted oracle build
# can leave a copy under DIR/work/oracle-* (or messy-oracle-*); work/ is scratch and may be deleted
# between runs. A failed messy or scale build removes its own derived copy, so only that step is repeated.
# Without --fixtures (or FERROFIN_ADOPTION_FIXTURES), DIR is $FERROFIN_GATES_DIR/fixtures/adoption-<key>,
# named for the files that build it, so changing one builds into a new directory by itself (lib.sh).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=gates/adoption/lib.sh
. "$HERE/lib.sh"
FIXTURES=${FERROFIN_ADOPTION_FIXTURES:-}
while [ $# -gt 0 ]; do case $1 in --fixtures) FIXTURES=$2; shift 2;; -h|--help) sed -n 2,38p "$0"; exit 0;; *) echo "unknown argument $1" >&2; exit 2;; esac; done
[ -n "$FIXTURES" ] || FIXTURES=$(adoption_fixtures_dir build-fixtures) || exit 2
gates_require_local "$FIXTURES" || exit 2
mkdir -p "$FIXTURES"; FIXTURES=$(cd "$FIXTURES" && pwd)
# Pull the release of every fixture still to build (and 10.11.8, whose FFmpeg encodes the
# media): a stale local tag would otherwise build a different release without complaint.
# Captured first: a failed command substitution in a for list would not stop the script.
versions=$(python3 - "$HERE" "$FIXTURES" <<'PY'
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from synthetic import VERSIONS
root = Path(sys.argv[2])
wanted = {version for name, version, _ in VERSIONS if not (root / name / "synthetic-ready.json").exists()}
if wanted or not (root / "media").exists():
    wanted.add("10.11.8")
print(*sorted(wanted))
PY
)
for version in $versions; do docker pull -q "jellyfin/jellyfin:$version" >/dev/null; done
python3 "$HERE/synthetic.py" build --fixtures "$FIXTURES"
MEDIA=(-v "$FIXTURES/media:/media:ro")

# Each fixture must carry exactly its release's EF history ids (the v12.2 C# gives 107 from
# 10.11, 108 from 12.0, 109 from 12.1, and 105 on a first run, which records none of the retired
# 10.11-era routines; 12.3 and 12.4 add none, so each of their routes records its 12.2
# counterpart's count), with the WAL folded in so read-only readers see it all.
for spec in 10.11.8:68 10.11.9:68 10.11.10:71 10.11.11:71 12.0:102 12.1-from-10:104 12.1-from-12:105 \
            12.2-from-10:107 12.2-from-12:108 12.2-from-121:109 12.2-fresh:105 \
            12.3-from-122:109 12.3-from-10:107 12.3-fresh:105 12.4-from-123:109 12.4-from-10:107 12.4-fresh:105; do
  db=$FIXTURES/jellyfin-${spec%:*}/data/jellyfin.db
  [ -f "$db" ] || { echo "jellyfin-${spec%:*} has no data/jellyfin.db" >&2; exit 1; }
  sqlite3 "$db" 'PRAGMA wal_checkpoint(TRUNCATE);' >/dev/null
  ids=$(sqlite3 -readonly "$db" 'SELECT COUNT(*) FROM __EFMigrationsHistory')
  [ "$ids" = "${spec#*:}" ] || { echo "jellyfin-${spec%:*} has $ids EF history ids, expected ${spec#*:}" >&2; exit 1; }
done
# The same count is not the same set: each 12.3 and 12.4 route must record exactly its 12.2
# counterpart's ids, the set crates/ferrofin-db adopts as generation 12.2.0. Any other set is a
# new generation, which the database gate must learn before these fixtures can test it.
ef_ids() { sqlite3 -readonly "$1" 'SELECT MigrationId FROM __EFMigrationsHistory ORDER BY 1'; }
for pair in 12.3-from-122:12.2-from-121 12.3-from-10:12.2-from-10 12.3-fresh:12.2-fresh \
            12.4-from-123:12.2-from-121 12.4-from-10:12.2-from-10 12.4-fresh:12.2-fresh; do
  diff -q <(ef_ids "$FIXTURES/jellyfin-${pair%:*}/data/jellyfin.db") <(ef_ids "$FIXTURES/jellyfin-${pair#*:}/data/jellyfin.db") >/dev/null \
    || { echo "jellyfin-${pair%:*} has other EF history ids than jellyfin-${pair#*:}: a new generation" >&2; exit 1; }
done

build_oracle() { # build_oracle <label> <fixture> <port> <image> — that image's own answers on a throwaway copy
  # label names the oracle file (smoke-jellyfin-<label>.txt); image is explicit, since a
  # fresh-install label (12.4-fresh) has no image tag of its own — a 12.4 image answers it.
  local version=$1 source=$FIXTURES/$2 port=$3 image=$4 user=${ADOPTION_USER:-}
  local out=$FIXTURES/oracle/smoke-jellyfin-$version.txt tmp
  case $version in 12.1|12.2|12.2-fresh|12.4|12.4-fresh) ;; *) echo "build_oracle: no oracle for $version" >&2; return 2;; esac
  [ ! -f "$out" ] || return 0
  # every oracle probes as the account run.sh probes as
  [ -n "$user" ] || [ ! -f "$FIXTURES/oracle/user.txt" ] || user=$(cat "$FIXTURES/oracle/user.txt")
  mkdir -p "$FIXTURES/oracle" "$FIXTURES/work"
  # a fresh directory per build: the clean-up below only ever removes what mktemp made
  tmp=$(mktemp -d "$FIXTURES/work/oracle-$version.XXXXXX")
  cp -a "$source" "$tmp/config"; mkdir "$tmp/cache"
  docker rm -f fixture-oracle >/dev/null 2>&1 || true
  docker run -d --name fixture-oracle --user "$(id -u):$(id -g)" -p "127.0.0.1:$port:8096" -v "$tmp/config:/config" -v "$tmp/cache:/cache" "${MEDIA[@]}" "$image" >/dev/null
  for _ in $(seq 1 300); do curl -sf "http://127.0.0.1:$port/System/Info/Public" >/dev/null && break; sleep 3; done; sleep 30
  # Write the oracle aside and move it into place only once smoke.sh succeeded: a partial
  # file would otherwise be skipped by the next run and fail every path against it.
  # smoke.sh exits 0 even when the server never came up (every probe "000"), so an oracle
  # with no answered probe is a failure too.
  if ! "$HERE/smoke.sh" "http://127.0.0.1:$port" "$tmp/config" "$user" > "$out.partial" \
    || ! grep -q '^200 ' "$out.partial"; then
    docker rm -f fixture-oracle >/dev/null 2>&1 || true
    rm -f -- "$out.partial"
    case $tmp in "$FIXTURES"/work/oracle-*) rm -rf -- "$tmp";; esac
    echo "build_oracle: smoke.sh failed for $version; nothing recorded" >&2; return 1
  fi
  mv -- "$out.partial" "$out"
  [ -f "$FIXTURES/oracle/user.txt" ] || sed -n 's/^200  \([^ ]*\) admin=.*\/Users\/Me$/\1/p' "$out" > "$FIXTURES/oracle/user.txt"
  docker rm -f fixture-oracle >/dev/null
  case $tmp in "$FIXTURES"/work/oracle-*) rm -rf -- "$tmp";; esac
  echo "oracle/smoke-jellyfin-$version.txt: $(wc -l < "$out") probes"
}

dump_schema() { # dump_schema <fixture dir> — the schema without SQLite's own bookkeeping tables
  # ANALYZE writes sqlite_stat1 and, on a build with SQLITE_ENABLE_STAT4 (Arch, Fedora, …),
  # sqlite_stat4 too; strip every sqlite_stat* and sqlite_sequence. Real fixtures never ANALYZE,
  # so this changes nothing there; it keeps the schema pin stable across host sqlite builds.
  sqlite3 -readonly "$1/data/jellyfin.db" .schema | grep -v -E '^CREATE TABLE (sqlite_stat[0-9]+|sqlite_sequence)'
}

# The latest release (12.4) is the reference every run.sh row answers by default; 12.1's and
# 12.2's oracles stay for --oracle.
build_oracle 12.1 jellyfin-12.1-from-12 18095 jellyfin/jellyfin:12.1
build_oracle 12.2 jellyfin-12.2-from-121 18085 jellyfin/jellyfin:12.2
build_oracle 12.2-fresh jellyfin-12.2-fresh 18084 jellyfin/jellyfin:12.2
build_oracle 12.4 jellyfin-12.4-from-123 18087 jellyfin/jellyfin:12.4
build_oracle 12.4-fresh jellyfin-12.4-fresh 18086 jellyfin/jellyfin:12.4
# same_schema <fixture> <its name for the reference> <reference schema file>: whether the fixture
# leaves exactly that schema.
same_schema() {
  if diff -q <(dump_schema "$FIXTURES/$1") "$3" >/dev/null; then
    echo "$1: same schema as $2"
  else
    echo "$1: schema DIFFERS from $2"
  fi
}
if [ -d "$FIXTURES/jellyfin-12.2-from-121" ]; then
  # The real 12.2 schema, for crates/ferrofin-db/tests/data/jellyfin-12.2-schema.sql; the other
  # routes must leave the same one, and so must 12.3's and 12.4's, which add no migration.
  dump_schema "$FIXTURES/jellyfin-12.2-from-121" > "$FIXTURES/jellyfin-12.2-schema.sql"
  echo "jellyfin-12.2-schema.sql: $(grep -c '^CREATE' "$FIXTURES/jellyfin-12.2-schema.sql") CREATE statements"
  for release in 12.3:jellyfin-12.3-from-122 12.4:jellyfin-12.4-from-123; do
    [ -d "$FIXTURES/${release#*:}" ] || continue
    dump_schema "$FIXTURES/${release#*:}" > "$FIXTURES/jellyfin-${release%%:*}-schema.sql"
  done
  for other in jellyfin-12.2-from-10 jellyfin-12.2-from-12 jellyfin-12.3-from-122 jellyfin-12.3-from-10 \
               jellyfin-12.4-from-123 jellyfin-12.4-from-10; do
    [ -d "$FIXTURES/$other" ] || continue
    same_schema "$other" jellyfin-12.2-schema.sql "$FIXTURES/jellyfin-12.2-schema.sql"
  done
fi
if [ -d "$FIXTURES/jellyfin-12.2-fresh" ]; then
  # A first run lacks one index the upgraded routes carry (crates/ferrofin-db's fresh-install
  # cases), so 12.3's and 12.4's fresh installs are diffed against 12.2's.
  for other in jellyfin-12.3-fresh jellyfin-12.4-fresh; do
    [ -d "$FIXTURES/$other" ] || continue
    same_schema "$other" jellyfin-12.2-fresh <(dump_schema "$FIXTURES/jellyfin-12.2-fresh")
  done
fi
# The messy copies of the latest 10.11.x and 12.x fixtures, with the end state Jellyfin 12.4 gave
# each shape (messy.py). Kept apart from the clean fixtures above, which stay the control.
python3 "$HERE/messy.py" build --fixtures "$FIXTURES"
# The scale copy of the 10.11.11 fixture: its media items cloned up to 40,000 by SQL (scale.py).
python3 "$HERE/scale.py" build --fixtures "$FIXTURES"
echo "fixtures ready under $FIXTURES"
