#!/usr/bin/env bash
# adoption/build-fixtures.sh --fixtures DIR
#
# Builds every fixture adoption/run.sh needs from synthetic data only — no real library, no
# media server, no network share. synthetic.py generates invented media under DIR/media and has
# each Jellyfin release build its own fixture through its API (see VERSIONS there):
#   jellyfin-10.11.8/                 Jellyfin 10.11.8 seeded from scratch
#   jellyfin-10.11.9/  10.11.10/  10.11.11/   that release booted and scanned on a copy of 10.11.8
#   jellyfin-12.0/  12.1-from-10/  12.1-from-12/  12.2-from-10/  12.2-from-12/  12.2-from-121/
#   jellyfin-12.2-fresh/              Jellyfin 12.2 seeded from scratch (a first run, not upgraded)
# Then, next to them:
#   oracle/smoke-jellyfin-12.1.txt smoke.sh against Jellyfin 12.1 on a throwaway copy of 12.1-from-12
#   oracle/smoke-jellyfin-12.2.txt smoke.sh against Jellyfin 12.2 on a throwaway copy of 12.2-from-121
#   oracle/smoke-jellyfin-12.2-fresh.txt  smoke.sh against Jellyfin 12.2 on a copy of 12.2-fresh
#   oracle/user.txt                the account both oracles (and run.sh) probe as
#   jellyfin-12.2-schema.sql       12.2-from-121's .schema without sqlite_stat*/sqlite_sequence,
#                                  rewritten every run; the other 12.2 routes are diffed against it
#   jellyfin-10.11.11-messy/  jellyfin-12.2-messy/   copies of 10.11.11 and 12.2-from-121 plus rows of
#                                  shapes no API produces, each with Jellyfin 12.2's end state for it
#                                  (messy.py: messy.json, messy-oracle.json)
# Existing fixtures and oracles are kept; synthetic.py refuses fixtures from an older recipe, so
# build into a new DIR after changing it. DIR must be on a local disk. An interrupted oracle build
# can leave a copy under DIR/work/oracle-* (or messy-oracle-*); work/ is scratch and may be deleted
# between runs. A failed messy build removes its own derived copy, so only that step is repeated.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=adoption/lib.sh
. "$HERE/lib.sh"
FIXTURES=${FERROFIN_ADOPTION_FIXTURES:-}
while [ $# -gt 0 ]; do case $1 in --fixtures) FIXTURES=$2; shift 2;; -h|--help) sed -n 2,24p "$0"; exit 0;; *) echo "unknown argument $1" >&2; exit 2;; esac; done
[ -n "$FIXTURES" ] || { echo "--fixtures DIR is required" >&2; exit 2; }
adoption_require_local "$FIXTURES" || exit 2
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
# 10.11-era routines), with the WAL folded in so read-only readers see it all.
for spec in 10.11.8:68 10.11.9:68 10.11.10:71 10.11.11:71 12.0:102 12.1-from-10:104 12.1-from-12:105 \
            12.2-from-10:107 12.2-from-12:108 12.2-from-121:109 12.2-fresh:105; do
  db=$FIXTURES/jellyfin-${spec%:*}/data/jellyfin.db
  [ -f "$db" ] || { echo "jellyfin-${spec%:*} has no data/jellyfin.db" >&2; exit 1; }
  sqlite3 "$db" 'PRAGMA wal_checkpoint(TRUNCATE);' >/dev/null
  ids=$(sqlite3 -readonly "$db" 'SELECT COUNT(*) FROM __EFMigrationsHistory')
  [ "$ids" = "${spec#*:}" ] || { echo "jellyfin-${spec%:*} has $ids EF history ids, expected ${spec#*:}" >&2; exit 1; }
done

build_oracle() { # build_oracle <label> <fixture> <port> <image> — that image's own answers on a throwaway copy
  # label names the oracle file (smoke-jellyfin-<label>.txt); image is explicit, since the
  # fresh-install label (12.2-fresh) has no image tag of its own — a 12.2 image answers it.
  local version=$1 source=$FIXTURES/$2 port=$3 image=$4 user=${ADOPTION_USER:-}
  local out=$FIXTURES/oracle/smoke-jellyfin-$version.txt tmp
  case $version in 12.1|12.2|12.2-fresh) ;; *) echo "build_oracle: no oracle for $version" >&2; return 2;; esac
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

build_oracle 12.1 jellyfin-12.1-from-12 18095 jellyfin/jellyfin:12.1
build_oracle 12.2 jellyfin-12.2-from-121 18085 jellyfin/jellyfin:12.2
build_oracle 12.2-fresh jellyfin-12.2-fresh 18084 jellyfin/jellyfin:12.2
if [ -d "$FIXTURES/jellyfin-12.2-from-121" ]; then
  # The real 12.2 schema, for crates/ferrofin-db/tests/data/jellyfin-12.2-schema.sql; the other
  # routes must leave the same one.
  dump_schema "$FIXTURES/jellyfin-12.2-from-121" > "$FIXTURES/jellyfin-12.2-schema.sql"
  echo "jellyfin-12.2-schema.sql: $(grep -c '^CREATE' "$FIXTURES/jellyfin-12.2-schema.sql") CREATE statements"
  for other in jellyfin-12.2-from-10 jellyfin-12.2-from-12; do
    [ -d "$FIXTURES/$other" ] || continue
    if diff -q <(dump_schema "$FIXTURES/$other") "$FIXTURES/jellyfin-12.2-schema.sql" >/dev/null; then
      echo "$other: same schema"
    else
      echo "$other: schema DIFFERS from jellyfin-12.2-schema.sql"
    fi
  done
fi
# The messy copies of the latest 10.11.x and 12.x fixtures, with the end state Jellyfin 12.2 gave
# each shape (messy.py). Kept apart from the clean fixtures above, which stay the control.
python3 "$HERE/messy.py" build --fixtures "$FIXTURES"
echo "fixtures ready under $FIXTURES"
