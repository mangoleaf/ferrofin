#!/usr/bin/env bash
# Functions shared by adoption/run.sh and adoption/smoke.sh. Sourced, never executed: nothing
# here touches docker or the network, which is what lets adoption/tests/*.bats drive every
# check with canned logs, smoke outputs and throwaway SQLite files.

# Probe answers that legitimately differ between Ferrofin and Jellyfin, or between two boots.
ADOPTION_IGNORE='/System/Info|/ScheduledTasks|/Plugins|/Devices|/Library/VirtualFolders|/System/ActivityLog|/Sessions'
# What a boot repair logs when it did something; a second boot must log none of these.
ADOPTION_REPAIR_RE='imported playlist|repaired |rewrote |recomputed |merged |backfilled |dropped dead|consolidat|reverted'

# adoption_require_local <dir>: refuse a directory on a network filesystem, judged by its nearest
# existing parent so nothing is created before the check. Every fixture is copied, booted and
# scanned several times; that load belongs on a local disk, never a NAS. findmnt names the real
# type (fuse.sshfs, not just fuse), so local FUSE disks such as ntfs-3g or mergerfs still pass.
adoption_require_local() {
  local dir=$1 types fs
  until [ -e "$dir" ]; do dir=$(dirname "$dir"); done
  types=$(findmnt -n -o FSTYPE -T "$dir") || { echo "refusing $1: cannot tell its filesystem" >&2; return 1; }
  for fs in $types; do
    case $fs in
      nfs*|cifs|smb*|9p|ceph|afs|glusterfs|davfs|fuse.sshfs|fuse.rclone|fuse.glusterfs|fuse.s3fs|fuse.davfs2)
        echo "refusing $1: it is on a network filesystem ($fs); use a local disk" >&2; return 1;;
    esac
  done
}

# adoption_normalise <smoke-file> [skip-re]: the comparable form of a smoke output — item ids
# redacted (they differ per copy), image byte counts dropped (encoders differ), ignored paths
# and the probes skip-re matches removed.
adoption_normalise() {
  sed -E 's/[0-9a-f]{32}/<id>/g; s/\(bytes=[0-9]+\)/(bytes)/' "$1" | grep -Ev "$ADOPTION_IGNORE${2:+|$2}"
}

# adoption_check_boot_log <server-log> <expected-generation>: one reason per line, nothing when
# the boot adopted the expected generation, applied every migration and logged no ERROR.
adoption_check_boot_log() {
  local log=$1 expected=$2 gen
  gen=$(grep -o '"generation":"[^"]*"' "$log" | head -1 | cut -d'"' -f4)
  [ "$gen" = "$expected" ] || echo "generation '$gen' != '$expected'"
  grep -q '"database migrations applied"' "$log" || echo "migrations did not complete"
  if grep -Eq '"level":"ERROR"' "$log"; then echo "ERROR in boot log"; fi
}

# adoption_compare_smoke <oracle> <smoke> [skip-re]: a reason when the normalised answers differ,
# leaving out the probes skip-re matches on both sides, naming the oracle's release
# (ORACLE_VERSION, which run.sh sets from --oracle).
adoption_compare_smoke() {
  local oracle=$1 smoke=$2 skip=${3:-}
  diff -q <(adoption_normalise "$oracle" "$skip") <(adoption_normalise "$smoke" "$skip") >/dev/null \
    || echo "smoke differs from Jellyfin ${ORACLE_VERSION:-12.1} (diff $oracle $smoke)"
}

# adoption_check_db <jellyfin.db>: integrity_check and foreign_key_check on the adopted file.
# IX_Peoples_NameLower is an expression index on lower("Name"); a host sqlite3 built with ICU
# lower-cases non-ASCII names differently from the SQLite inside Jellyfin and Ferrofin, so it
# reports those rows "missing from index" on a file both servers agree with. Only that index is
# exempt.
adoption_check_db() {
  local db=$1 ic fk
  # Capture sqlite3's status before filtering: a failed query must produce a
  # reason on stdout, which run.sh collects into its failure list.
  if ic=$(sqlite3 -readonly "file:$db?mode=ro" 'PRAGMA integrity_check' 2>&1); then
    ic=$(printf '%s\n' "$ic" | grep -Ev '^(ok|row [0-9]+ missing from index IX_Peoples_NameLower)$' || true)
    [ -z "$ic" ] || echo "integrity_check: $ic"
  else
    echo "integrity_check: sqlite3 failed: $ic"
  fi
  if fk=$(sqlite3 -readonly "file:$db?mode=ro" 'PRAGMA foreign_key_check' 2>&1); then
    [ -z "$fk" ] || echo "foreign_key_check: $(printf '%s\n' "$fk" | wc -l) rows"
  else
    echo "foreign_key_check: sqlite3 failed: $fk"
  fi
}

# adoption_second_boot_repairs <log-since-restart>: the first repair line a second boot logged,
# or nothing. Ignore a repair only when all its numeric counters are zero;
# alternate-version repairs can promote items even when "repaired" is zero.
# A repair with no counters is conservatively reported.
adoption_second_boot_repairs() {
  jq -Rr --arg re "$ADOPTION_REPAIR_RE" '
    fromjson?
    | select((.fields.message // "") | test($re))
    | [.fields[] | select(type == "number")] as $counts
    | select(($counts | length) == 0 or any($counts[]; . != 0))
    | tojson
  ' "$1" | head -1 | cut -c1-120
}

# adoption_smoke_credentials <jellyfin.db> [username]: "username token deviceid userid" for the
# probes — the named account, else the administrator with the most recently active "Jellyfin
# Web" session (the same account on every copy of one library). Fails when no such session
# exists, since half the probes need one.
adoption_smoke_credentials() {
  local db=$1 want=${2:-} where row
  # PermissionKind.IsAdministrator = 0
  if [ -n "$want" ]; then where="u.Username = '$want'"; else where="EXISTS (SELECT 1 FROM Permissions p WHERE p.UserId = u.Id AND p.Kind = 0 AND p.Value = 1)"; fi
  row=$(sqlite3 -readonly "file:$db?mode=ro" "SELECT u.Username, d.AccessToken, d.DeviceId, lower(replace(u.Id,'-','')) FROM Devices d JOIN Users u ON u.Id = d.UserId WHERE $where AND d.AppName = 'Jellyfin Web' ORDER BY d.DateLastActivity DESC LIMIT 1")
  [ -n "$row" ] || { echo "no Jellyfin Web session for ${want:-an administrator} in $db" >&2; return 1; }
  tr '|' ' ' <<<"$row"
}
