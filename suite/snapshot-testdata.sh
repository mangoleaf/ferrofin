#!/usr/bin/env bash
# Pin a real server's data directory as suite test data.
#
#   suite/snapshot-testdata.sh                      # default source -> suite/test_data/
#   suite/snapshot-testdata.sh --src DIR --dest DIR
#   suite/snapshot-testdata.sh --refresh            # replace an existing snapshot
#
# WHY: the suite's synthetic fixtures make most endpoints return empty or
# near-empty bodies — measured over 240 captured pairs, the median endpoint had
# 13 fields actually compared and 20 had ZERO, which makes a "clean" verdict
# nearly meaningless. A real, populated library is the only thing that fixes
# that. Ferrofin's schema is pinned byte-equal to Jellyfin 10.11.8, so ONE real
# directory can boot both servers (that is what suite/adopt-live.sh proves).
#
# The snapshot is PINNED, not re-taken per run: a data dir that changes every
# run changes the fixture hash, which resets the shape baseline and invalidates
# the perf baseline every single time. Refresh it deliberately, like a rebaseline.
#
# THE SOURCE IS NEVER WRITTEN TO. The database is read through a read-only
# handle, everything else is copied outward, and the script refuses to run if
# the destination is inside the source.
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(cd .. && pwd)"

# Default source: the live JELLYFIN instance — the settled direction (plan §E).
# A Ferrofin-lived directory is a disproven dead end (§E0): its legacy item-id
# derivation makes Jellyfin serve zero items from it.
SRC=${TESTDATA_SRC:-/mnt/nvme0/k3s/jellyfin-config}
DEST=${TESTDATA_DEST:-$ROOT/suite/test_data}
REFRESH=0
while [ $# -gt 0 ]; do
  case "$1" in
    --src)     SRC=$2; shift 2;;
    --dest)    DEST=$2; shift 2;;
    --refresh) REFRESH=1; shift;;
    -h|--help) sed -n '2,7p' "$0"; exit 0;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

SRC=${SRC%/}; DEST=${DEST%/}
[ -d "$SRC" ] || { echo "source is not a directory: $SRC" >&2; exit 1; }

# Refuse to write into the source, at any depth. The whole contract of this
# script is that a real, live instance survives it untouched. Resolution must
# HARD-FAIL when it cannot answer: `readlink -f` on a dest whose parent does
# not exist yet returns empty, and an empty word here would match nothing and
# wave the write through — into the live source. (`-m` resolves nonexistent
# paths; the emptiness checks keep the guard fail-closed on any readlink
# surprise.) Note args are resolved relative to suite/ (the cd above), so use
# absolute --src/--dest paths.
SRC_R=$(readlink -f "$SRC") && [ -n "$SRC_R" ] || { echo "cannot resolve source path: $SRC" >&2; exit 1; }
DEST_R=$(readlink -m "$DEST") && [ -n "$DEST_R" ] || { echo "cannot resolve destination path: $DEST" >&2; exit 1; }
case "$DEST_R/" in
  "$SRC_R/"*) echo "refusing: destination is inside the source ($SRC)" >&2; exit 1;;
esac
# …and the other direction: a DEST that is an ANCESTOR of the source would,
# under --refresh, chmod and displace the live tree itself.
case "$SRC_R/" in
  "$DEST_R/"*) echo "refusing: source is inside the destination ($DEST)" >&2; exit 1;;
esac
SRC=$SRC_R; DEST=$DEST_R   # operate on what the guard actually validated

# Which database? Ferrofin keeps ferrofin.db at the top level; Jellyfin keeps
# data/jellyfin.db. adopt-live.sh only ever knew the Jellyfin shape, which is
# why it could not read a real Ferrofin config directory at all.
if   [ -f "$SRC/ferrofin.db" ];      then DB_SRC="$SRC/ferrofin.db";      DB_REL="ferrofin.db";      LAYOUT=ferrofin
elif [ -f "$SRC/data/jellyfin.db" ]; then DB_SRC="$SRC/data/jellyfin.db"; DB_REL="data/jellyfin.db"; LAYOUT=jellyfin
else echo "no ferrofin.db or data/jellyfin.db under $SRC" >&2; exit 1; fi

if [ -e "$DEST" ] && [ "$REFRESH" != 1 ]; then
  echo "$DEST already exists — it is PINNED on purpose. Pass --refresh to replace it." >&2
  exit 1
fi
# --refresh only ever replaces a directory this script made. Without this, a
# mistyped --dest pointing at a directory the caller cares about would be
# renamed away and deleted as if it were an old snapshot.
if [ -e "$DEST" ] && [ ! -f "$DEST/MANIFEST.json" ]; then
  echo "refusing --refresh: $DEST exists but has no MANIFEST.json — not a snapshot this script made" >&2
  exit 1
fi

echo ">> source : $SRC  (layout: $LAYOUT, db: $DB_REL)"
echo ">> dest   : $DEST"
echo ">> size   : $(du -sh "$SRC" 2>/dev/null | cut -f1) to copy"

# Stage beside the destination and swap at the end, so an interrupted run never
# leaves a half-copied directory that looks like a valid snapshot.
STAGE="$DEST.staging.$$"
# Recursive deletes in this script go through this guard: it refuses any path
# that is not a staging/old directory THIS process named. A variable path fed
# straight to `rm -rf` is how a benchmark script eats a library.
remove_own_dir() {
  case "$1" in
    # chmod first: copies of a read-only pin carry r-x dirs that rm cannot descend.
    "$DEST.staging.$$"|"$DEST.old.$$") chmod -R u+w -- "$1" 2>/dev/null || true; rm -rf -- "$1";;
    *) echo "refusing to delete unexpected path: $1" >&2; exit 1;;
  esac
}
trap 'remove_own_dir "$STAGE"' EXIT
mkdir -p "$STAGE"

echo ">> copying everything except the database"
# Enumerate FIRST and check the status: a failed opendir inside a process
# substitution would otherwise look like an empty source and swap in a pin
# with a valid MANIFEST and no file tree. Then any cp failure is FATAL
# (set -e): a swallowed ENOSPC or I/O error mid-way through the metadata tree
# is the same silent-partial-pin failure. Volatile runtime dirs are excluded —
# they churn on a live server (fatal-cp flakes) and are not fixture data.
ENTRIES=$(find "$SRC" -mindepth 1 -maxdepth 1) || { echo "cannot enumerate $SRC" >&2; exit 1; }
[ -n "$ENTRIES" ] || { echo "source is empty: $SRC" >&2; exit 1; }
while IFS= read -r entry; do
  case "$(basename "$entry")" in
    log|temp|transcodes|cache) continue;;   # runtime junk, churns while live
  esac
  cp -a "$entry" "$STAGE/"
done <<<"$ENTRIES"
# The database is .backup'd below, whatever the layout — remove any file-copied
# version (top-level ferrofin layout OR nested jellyfin data/ layout) so a torn
# copy of a live db can never survive into the pin.
rm -f -- "$STAGE/$DB_REL" "$STAGE/$DB_REL-wal" "$STAGE/$DB_REL-shm"

# The database gets a CONSISTENT snapshot, not a file copy. A live server holds
# it open with a write-ahead log; `cp` captures the main file without whatever
# is still in the WAL and yields a torn database. `.backup` through a read-only
# handle folds the WAL in and cannot write to the source.
echo ">> snapshotting the database (read-only .backup, WAL folded in)"
mkdir -p "$STAGE/$(dirname "$DB_REL")"
sqlite3 "file:${DB_SRC}?mode=ro" ".backup '$STAGE/$DB_REL'"
rm -f -- "$STAGE/$DB_REL-wal" "$STAGE/$DB_REL-shm"   # .backup output needs neither

echo ">> verifying the snapshot"
[ "$(sqlite3 "$STAGE/$DB_REL" 'PRAGMA integrity_check;' | head -1)" = ok ] \
  || { echo "the snapshot is not a consistent database" >&2; exit 1; }

items=$(sqlite3 "$STAGE/$DB_REL" 'SELECT count(*) FROM BaseItems;' 2>/dev/null || echo 0)
users=$(sqlite3 "$STAGE/$DB_REL" 'SELECT count(*) FROM Users;' 2>/dev/null || echo 0)
sha=$(sha256sum "$STAGE/$DB_REL" | cut -d' ' -f1)

# The snapshot itself is gitignored (tens of GB, and it carries real usernames
# and watch history). This manifest IS committed, so the fixture in use is
# identifiable and a mismatch is detectable without committing any of it.
cat > "$STAGE/MANIFEST.json" <<JSON
{
  "source": "$SRC",
  "layout": "$LAYOUT",
  "database": "$DB_REL",
  "db_sha256": "$sha",
  "base_items": $items,
  "users": $users,
  "taken": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "note": "Identifies the pin only. The snapshot DATA is never committed or published; refresh with suite/snapshot-testdata.sh --refresh"
}
JSON

# The stage is COMPLETE — clear the trap BEFORE displacing DEST, so a failure
# in the swap window can never delete the finished stage (worst case: loud
# failure with the new snapshot at .staging.$$ and the old at .old.$$, nothing
# lost). `mv -T` fails instead of nesting if DEST reappeared concurrently.
# (`[ -e ] && …` as a bare statement would exit-1 the whole script under
# `set -e` on the fresh-pin path, so these are explicit ifs.)
trap - EXIT
if [ -e "$DEST" ]; then chmod -R u+w -- "$DEST"; mv -T -- "$DEST" "$DEST.old.$$"; fi
mv -T -- "$STAGE" "$DEST"
if [ -e "$DEST.old.$$" ]; then remove_own_dir "$DEST.old.$$"; fi

# Pin it: the snapshot is a fixture, and nothing may write to it in place.
# Consumers copy it out per run (Ferrofin's adoption MUTATES the database).
chmod -R a-w -- "$DEST"

echo ">> pinned: $DEST"
echo "   $items items, $users users, db sha256 ${sha:0:16}…"
echo "   commit suite/test_data/MANIFEST.json; the data itself is gitignored"
