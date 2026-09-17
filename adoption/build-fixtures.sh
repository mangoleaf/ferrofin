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
#   oracle/smoke-jellyfin-12.1.txt smoke.sh against Jellyfin 12.1 on a throwaway copy
# Existing outputs are kept; delete one to rebuild it. Needs docker and ~4× the snapshot's size.
# If the library options reference media paths, put a media-mounts.sh in DIR that sets
# MEDIA=(-v host:container:ro …); it is sourced here and by run.sh.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
FIXTURES=${FERROFIN_ADOPTION_FIXTURES:-}
while [ $# -gt 0 ]; do case $1 in --fixtures) FIXTURES=$2; shift 2;; -h|--help) sed -n 2,16p "$0"; exit 0;; *) echo "unknown argument $1" >&2; exit 2;; esac; done
[ -n "$FIXTURES" ] || { echo "--fixtures DIR is required" >&2; exit 2; }
FIXTURES=$(cd "$FIXTURES" && pwd); SRC=$FIXTURES/jellyfin-10.11.8
[ -f "$SRC/data/jellyfin.db" ] || { echo "no $SRC/data/jellyfin.db" >&2; exit 2; }
MEDIA=()
# shellcheck disable=SC1091 # user-supplied, outside the repository
[ ! -f "$FIXTURES/media-mounts.sh" ] || . "$FIXTURES/media-mounts.sh"
count=$(sqlite3 -readonly "file:$SRC/data/jellyfin.db?immutable=1" 'SELECT COUNT(*) FROM __EFMigrationsHistory')
[ "$count" = 68 ] || { echo "$SRC has $count EF migration ids, a 10.11.8 database has 68" >&2; exit 2; }

boot_jellyfin() { # boot_jellyfin <image> <dir> <port> — one boot, wait, let startup tasks run, stop cleanly
  local image=$1 dir=$2 port=$3
  local name=fixture-$port
  mkdir -p "$dir-cache"; docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --name "$name" --user "$(id -u):$(id -g)" -p "127.0.0.1:$port:8096" -v "$dir:/config" -v "$dir-cache:/cache" "${MEDIA[@]}" "$image" >/dev/null
  for _ in $(seq 1 600); do curl -sf "http://127.0.0.1:$port/System/Info/Public" >/dev/null && break; sleep 3; done
  sleep 90
  docker logs "$name" > "$dir-migration.log" 2>&1; docker stop -t 60 "$name" >/dev/null; docker rm "$name" >/dev/null
  sqlite3 "$dir/data/jellyfin.db" 'PRAGMA wal_checkpoint(TRUNCATE);' >/dev/null
  echo "$(basename "$dir"): $(sqlite3 "$dir/data/jellyfin.db" 'SELECT COUNT(*) FROM __EFMigrationsHistory') EF ids"
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
if [ ! -f "$FIXTURES/oracle/smoke-jellyfin-12.1.txt" ]; then
  # Jellyfin 12.1's own answers on a throwaway copy of the fullest fixture: the comparison target.
  mkdir -p "$FIXTURES/oracle"; tmp=$FIXTURES/work/oracle-12.1; rm -rf "$tmp" "$tmp-cache"; mkdir -p "$FIXTURES/work"
  cp -a "$FIXTURES/jellyfin-12.1-from-12" "$tmp"; mkdir -p "$tmp-cache"
  docker rm -f fixture-oracle >/dev/null 2>&1 || true
  docker run -d --name fixture-oracle --user "$(id -u):$(id -g)" -p 127.0.0.1:18095:8096 -v "$tmp:/config" -v "$tmp-cache:/cache" "${MEDIA[@]}" jellyfin/jellyfin:12.1 >/dev/null
  for _ in $(seq 1 300); do curl -sf http://127.0.0.1:18095/System/Info/Public >/dev/null && break; sleep 3; done; sleep 30
  "$HERE/smoke.sh" http://127.0.0.1:18095 "$tmp" "${ADOPTION_USER:-}" > "$FIXTURES/oracle/smoke-jellyfin-12.1.txt"
  # run.sh probes every fixture as this same account
  sed -n 's/^200  \([^ ]*\) admin=.*\/Users\/Me$/\1/p' "$FIXTURES/oracle/smoke-jellyfin-12.1.txt" > "$FIXTURES/oracle/user.txt"
  docker rm -f fixture-oracle >/dev/null; rm -rf "$tmp" "$tmp-cache"
  echo "oracle/smoke-jellyfin-12.1.txt: $(wc -l < "$FIXTURES/oracle/smoke-jellyfin-12.1.txt") probes"
fi
echo "fixtures ready under $FIXTURES"
