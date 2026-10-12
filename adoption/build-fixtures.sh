#!/usr/bin/env bash
# adoption/build-fixtures.sh --fixtures DIR
#
# Derives every fixture adoption/run.sh needs from ONE supplied Jellyfin 10.11.8 data directory
# at DIR/jellyfin-10.11.8 (config/, data/jellyfin.db, root/, metadata/ …; the database should be
# a checkpointed copy, e.g. `sqlite3 jellyfin.db ".backup jellyfin.db"`). Nothing here touches
# that snapshot. Produces, next to it:
#   jellyfin-10.11.9/  10.11.10/  10.11.11/   one boot of that jellyfin/jellyfin release on a copy
#   jellyfin-12.0/                 one boot of jellyfin/jellyfin:12.0 on a copy
#   jellyfin-12.1-from-10/         one boot of jellyfin/jellyfin:12.1 on a copy of 10.11.8
#   jellyfin-12.1-from-12/         one boot of jellyfin/jellyfin:12.1 on a copy of 12.0
#   jellyfin-12.2-from-10/         one boot of jellyfin/jellyfin:12.2 on a copy of 10.11.8
#   jellyfin-12.2-from-12/         one boot of jellyfin/jellyfin:12.2 on a copy of 12.0
#   jellyfin-12.2-from-121/        one boot of jellyfin/jellyfin:12.2 on a copy of 12.1-from-12
#   oracle/smoke-jellyfin-12.1.txt smoke.sh against Jellyfin 12.1 on a throwaway copy of 12.1-from-12
#   oracle/smoke-jellyfin-12.2.txt smoke.sh against Jellyfin 12.2 on a throwaway copy of 12.2-from-121
#   oracle/user.txt                the account both oracles (and run.sh) probe as
#   jellyfin-12.2-schema.sql       12.2-from-121's .schema without sqlite_stat1/sqlite_sequence,
#                                  rewritten every run; the other 12.2 routes are diffed against it
# Every boot prints its EF history-id count. Existing fixtures and oracles are kept; delete one
# to rebuild it. Needs docker and ~7× the snapshot's size. An interrupted oracle build can leave
# a copy under DIR/work/oracle-*; work/ is scratch and may be deleted between runs.
# If the library options reference media paths, put a media-mounts.sh in DIR that sets
# MEDIA=(-v host:container:ro …), or point ADOPTION_MEDIA_MOUNTS at one; it is sourced here
# and by run.sh.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
FIXTURES=${FERROFIN_ADOPTION_FIXTURES:-}
while [ $# -gt 0 ]; do case $1 in --fixtures) FIXTURES=$2; shift 2;; -h|--help) sed -n 2,25p "$0"; exit 0;; *) echo "unknown argument $1" >&2; exit 2;; esac; done
[ -n "$FIXTURES" ] || { echo "--fixtures DIR is required" >&2; exit 2; }
FIXTURES=$(cd "$FIXTURES" && pwd); SRC=$FIXTURES/jellyfin-10.11.8
[ -f "$SRC/data/jellyfin.db" ] || { echo "no $SRC/data/jellyfin.db" >&2; exit 2; }
MEDIA=(); MOUNTS=${ADOPTION_MEDIA_MOUNTS:-$FIXTURES/media-mounts.sh}
[ -z "${ADOPTION_MEDIA_MOUNTS:-}" ] || [ -f "$MOUNTS" ] || { echo "no $MOUNTS" >&2; exit 2; }
# shellcheck disable=SC1090,SC1091 # user-supplied, outside the repository
[ ! -f "$MOUNTS" ] || . "$MOUNTS"
count=$(sqlite3 -readonly "file:$SRC/data/jellyfin.db?immutable=1" 'SELECT COUNT(*) FROM __EFMigrationsHistory')
[ "$count" = 68 ] || { echo "$SRC has $count EF migration ids, a 10.11.8 database has 68" >&2; exit 2; }

boot_jellyfin() { # boot_jellyfin <image> <dir> <port> [expected-ids] — one boot, wait, let startup tasks run, stop cleanly
  local image=$1 dir=$2 port=$3 expected=${4:-} ids
  local name=fixture-$port
  mkdir -p "$dir-cache"; docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --name "$name" --user "$(id -u):$(id -g)" -p "127.0.0.1:$port:8096" -v "$dir:/config" -v "$dir-cache:/cache" "${MEDIA[@]}" "$image" >/dev/null
  for _ in $(seq 1 600); do curl -sf "http://127.0.0.1:$port/System/Info/Public" >/dev/null && break; sleep 3; done
  sleep 90
  docker logs "$name" > "$dir-migration.log" 2>&1; docker stop -t 60 "$name" >/dev/null; docker rm "$name" >/dev/null
  sqlite3 "$dir/data/jellyfin.db" 'PRAGMA wal_checkpoint(TRUNCATE);' >/dev/null
  ids=$(sqlite3 "$dir/data/jellyfin.db" 'SELECT COUNT(*) FROM __EFMigrationsHistory')
  echo "$(basename "$dir"): $ids EF ids"
  [ -z "$expected" ] || [ "$ids" = "$expected" ] || echo "MISMATCH: $(basename "$dir") has $ids EF ids, expected $expected"
}

build_oracle() { # build_oracle <version> <fixture> <port> — that release's own answers on a throwaway copy
  local version=$1 source=$FIXTURES/$2 port=$3 user=${ADOPTION_USER:-}
  local out=$FIXTURES/oracle/smoke-jellyfin-$version.txt tmp
  case $version in 12.1|12.2) ;; *) echo "build_oracle: no oracle for $version" >&2; return 2;; esac
  [ ! -f "$out" ] || return 0
  # every oracle probes as the account run.sh probes as
  [ -n "$user" ] || [ ! -f "$FIXTURES/oracle/user.txt" ] || user=$(cat "$FIXTURES/oracle/user.txt")
  mkdir -p "$FIXTURES/oracle" "$FIXTURES/work"
  # a fresh directory per build: the clean-up below only ever removes what mktemp made
  tmp=$(mktemp -d "$FIXTURES/work/oracle-$version.XXXXXX")
  cp -a "$source" "$tmp/config"; mkdir "$tmp/cache"
  docker rm -f fixture-oracle >/dev/null 2>&1 || true
  docker run -d --name fixture-oracle --user "$(id -u):$(id -g)" -p "127.0.0.1:$port:8096" -v "$tmp/config:/config" -v "$tmp/cache:/cache" "${MEDIA[@]}" "jellyfin/jellyfin:$version" >/dev/null
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
  sqlite3 -readonly "$1/data/jellyfin.db" .schema | grep -v -E '^CREATE TABLE (sqlite_stat1|sqlite_sequence)'
}

# Every 10.11 point release Ferrofin adopts, each a real boot of that release on a copy of the
# snapshot: 10.11.9 leaves the 68 ids as they are; 10.11.10 and 10.11.11 add the three
# NormalizedUsername ids (71).
for v in 10.11.9:18089 10.11.10:18088 10.11.11:18098; do
  ver=${v%%:*}; port=${v#*:}
  [ -d "$FIXTURES/jellyfin-$ver" ] && continue
  docker pull -q "jellyfin/jellyfin:$ver"; cp -a "$SRC" "$FIXTURES/jellyfin-$ver"; boot_jellyfin "jellyfin/jellyfin:$ver" "$FIXTURES/jellyfin-$ver" "$port"
done
if [ ! -d "$FIXTURES/jellyfin-12.0" ]; then docker pull -q jellyfin/jellyfin:12.0; cp -a "$SRC" "$FIXTURES/jellyfin-12.0"; boot_jellyfin jellyfin/jellyfin:12.0 "$FIXTURES/jellyfin-12.0" 18096; fi
if [ ! -d "$FIXTURES/jellyfin-12.1-from-10" ]; then docker pull -q jellyfin/jellyfin:12.1; cp -a "$SRC" "$FIXTURES/jellyfin-12.1-from-10"; boot_jellyfin jellyfin/jellyfin:12.1 "$FIXTURES/jellyfin-12.1-from-10" 18094; fi
if [ ! -d "$FIXTURES/jellyfin-12.1-from-12" ]; then docker pull -q jellyfin/jellyfin:12.1; cp -a "$FIXTURES/jellyfin-12.0" "$FIXTURES/jellyfin-12.1-from-12"; boot_jellyfin jellyfin/jellyfin:12.1 "$FIXTURES/jellyfin-12.1-from-12" 18095; fi
# Jellyfin 12.2 on each upgrade route, from the parent the route names. Its history ids, derived
# from the v12.2 C#: 107 from 10.11, 108 from 12.0, 109 from 12.1 (itself from 12.0).
for spec in jellyfin-12.2-from-10:jellyfin-10.11.8:18087:107 jellyfin-12.2-from-12:jellyfin-12.0:18086:108 \
            jellyfin-12.2-from-121:jellyfin-12.1-from-12:18085:109; do
  IFS=: read -r name parent port expected <<<"$spec"
  [ -d "$FIXTURES/$name" ] && continue
  docker pull -q jellyfin/jellyfin:12.2; cp -a "$FIXTURES/$parent" "$FIXTURES/$name"; boot_jellyfin jellyfin/jellyfin:12.2 "$FIXTURES/$name" "$port" "$expected"
done
build_oracle 12.1 jellyfin-12.1-from-12 18095
build_oracle 12.2 jellyfin-12.2-from-121 18085
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
echo "fixtures ready under $FIXTURES"
