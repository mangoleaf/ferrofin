#!/usr/bin/env python3
"""Compare every Jellyfin-owned table the other checks leave alone, row by row, before and after.

Each row is hashed whole, keyed by its primary key. A baseline taken on the source database is
checked against the adopted one at every stage: a row that is gone, or whose hash moved, fails
with the table and key named. Rows only the later database holds are not failures: a scan or a
login legitimately adds them (a first 12.x scan makes music-artist credits).

The normaliser removes only what the migration chain rewrites by design. Every rule below is a
change the check stops seeing, so a rule needs a migration to point at.
"""

import argparse
from collections import Counter
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import sys
import uuid

from metadata import CheckError
from watch_history import timestamp


TABLES = (
    "MediaStreamInfos", "AttachmentStreamInfos", "Chapters", "TrickplayInfos", "MediaSegments",
    "KeyframeData", "ItemValues", "ItemValuesMap", "Peoples", "PeopleBaseItemMap",
    "DisplayPreferences", "ItemDisplayPreferences", "CustomItemDisplayPreferences",
    "Devices", "DeviceOptions", "ApiKeys", "ActivityLogs",
)
# Columns dropped before hashing, with the reason.
IGNORED = {
    "ItemValues": ("CleanValue",),  # 0031 re-derives keys that matched the previous derivation
    # Every authenticated request re-announces these, so they move on any device the harness
    # touches. The device's own identity (Id, token, activation, creation date) is kept and
    # compared below, so a reset token or a deactivated device fails.
    "Devices": ("AppName", "AppVersion", "DeviceName", "DateModified", "DateLastActivity"),
    "ApiKeys": ("DateLastActivity",),
}
# Dropped only for the DeviceIds the harness itself logs in from (reauthenticated_devices):
# authenticating again from one replaces its row with a new Id, token, IsActive and creation
# date, as Jellyfin does. On every other device these are compared, so an adoption bug that
# reset a token or deactivated a device — logging that client out — fails the check.
REAUTH_VOLATILE = ("Id", "AccessToken", "IsActive", "DateCreated")
# 0007 rewrites ISO 'T' timestamps to the space form; both spell one instant.
TIMESTAMPS = {
    "ActivityLogs": ("DateCreated",),
    "ApiKeys": ("DateCreated",),
    "Chapters": ("ImageDateModified",),
    "Devices": ("DateCreated",),  # compared on devices the harness did not re-authenticate
}
# Bumped when the baseline's shape changes so a stale file is rejected, not mis-read. 2: rows
# sharing a key became a list (multiset).
SNAPSHOT_VERSION = 2
KEYS = {"Devices": ("UserId", "DeviceId")}
# Tables whose ItemId names a library item. The rehome repair re-keys a local alternate version
# to the id its primary's type derives, as Jellyfin 12.2 does, so an item is identified by its
# unique path instead of its id (identities()).
ITEM_KEYED = ("MediaStreamInfos", "AttachmentStreamInfos", "Chapters", "TrickplayInfos", "MediaSegments",
              "KeyframeData", "ItemValuesMap", "PeopleBaseItemMap", "DisplayPreferences",
              "ItemDisplayPreferences", "CustomItemDisplayPreferences")
# Only an owned item (a version or extra) is re-identified, so a library-wide re-key still fails.
# These two ids mean "no owner" (0032).
NO_OWNER = ("00000000000000000000000000000000", "00000000000000000000000000000001")
# 0044 deletes the rows of a user that no longer exists, so they are not in the baseline.
USER_OWNED = ("DisplayPreferences", "ItemDisplayPreferences", "CustomItemDisplayPreferences", "Devices")


def normalize(value):
    """A GUID spelled any way (0007, 0032 casing and dashes) as 32 lowercase hex digits."""
    if isinstance(value, str) and len(value) in (32, 36):
        try:
            return uuid.UUID(value).hex
        except ValueError:
            pass
    return value


def digest(row):
    return hashlib.sha256(json.dumps(row, sort_keys=True, default=repr).encode()).hexdigest()


def reauthenticated_devices(users, passwordless):
    """The DeviceIds the synthetic runner authenticates from after taking the baseline, whose
    device rows it legitimately replaces: ``synthetic_fixture.Api.login`` logs in from
    ``synthetic-login-<user>`` and ``user_accounts.snapshot`` from ``account-login-<user>`` for
    every user and the passwordless account. ``run.sh`` authenticates from none of these (it
    replays a stored token), so it compares every device's full row."""
    return {"synthetic-login-" + name for name in users} | {
        "account-login-" + name for name in (*users, passwordless)
    }


def identities(conn):
    """{item id: 'path:<Path>'} for every owned item whose path no other item shares."""
    items = [(normalize(item), path, normalize(owner))
             for item, path, owner in conn.execute('SELECT "Id", "Path", "OwnerId" FROM "BaseItems"')]
    counts = Counter(path for _, path, _ in items)
    return {item: "path:" + path for item, path, owner in items
            if path and counts[path] == 1 and owner and owner not in NO_OWNER}


def table_rows(conn, table, only=None, identity=None, users=None, reauthenticated=frozenset()):
    """(columns, {key: sorted row hashes}) of one table, normalised; `only` limits the hashed
    columns. Rows that share a key are kept as a multiset so losing one of them still fails."""
    info = list(conn.execute(f'PRAGMA table_info("{table}")'))
    if not info:
        raise CheckError(f"table {table} is missing")
    columns = [row[1] for row in info if row[1] not in IGNORED.get(table, ()) and (only is None or row[1] in only)]
    keys = KEYS.get(table) or [row[1] for row in sorted(info, key=lambda row: row[5]) if row[5]]
    if not keys:
        raise CheckError(f"table {table} has no primary key to name its rows by")
    rows = {}
    for record in conn.execute(f'SELECT * FROM "{table}"'):
        if table in USER_OWNED and record["UserId"] is not None and normalize(record["UserId"]) not in users:
            continue
        row = {name: normalize(record[name]) for name in columns}
        if table == "Devices" and record["DeviceId"] in reauthenticated:
            for name in REAUTH_VOLATILE:
                row.pop(name, None)
        if table in ITEM_KEYED and identity and "ItemId" in row:
            row["ItemId"] = identity.get(row["ItemId"], row["ItemId"])
        for name in TIMESTAMPS.get(table, ()):
            if not isinstance(row.get(name), str):
                continue
            try:
                row[name] = timestamp(row[name])
            except ValueError:
                pass  # not a date: hashed as stored, so any change to it still shows
        key = "/".join(str(row[name]) for name in keys)
        rows.setdefault(key, []).append(digest(row))
    return columns, {key: sorted(digests) for key, digests in rows.items()}


def snapshot(db, baseline=None, reauthenticated=frozenset()):
    """Every table's rows. With a baseline, only the columns it holds (0032 adds some).
    `reauthenticated` names the DeviceIds the harness logs in from (reauthenticated_devices);
    on a path that re-authenticates, baseline and comparison snapshots must pass the same set."""
    reauthenticated = frozenset(reauthenticated)
    with closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as conn:
        conn.row_factory = sqlite3.Row
        conn.execute("BEGIN")
        identity = identities(conn)
        users = {normalize(user) for (user,) in conn.execute('SELECT "Id" FROM "Users"')}
        return {"version": SNAPSHOT_VERSION, "tables": {
            table: dict(zip(("columns", "rows"), table_rows(
                conn, table, baseline and baseline["tables"][table]["columns"], identity, users,
                reauthenticated))) for table in TABLES}}


def compare(before, after):
    """Failures naming table and key; a column the baseline holds that is gone is named once."""
    failures = []
    for table, was in before["tables"].items():
        now = after["tables"][table]
        gone = [name for name in was["columns"] if name not in now["columns"]]
        if gone:
            failures.append(f"{table} lost column {', '.join(gone)}")
            continue
        for key, before_digests in was["rows"].items():
            after_digests = now["rows"].get(key)
            if after_digests is None:
                failures.append(f"{table} row {key} is gone")
            elif len(before_digests) == 1 and len(after_digests) == 1:
                if before_digests[0] != after_digests[0]:
                    failures.append(f"{table} row {key} changed")
            else:
                # Rows sharing a key are a multiset: a lost or changed one leaves a digest
                # the after side can no longer account for.
                lost = Counter(before_digests) - Counter(after_digests)
                if lost:
                    failures.append(
                        f"{table} row {key}: {sum(lost.values())} of {len(before_digests)} rows sharing its key gone or changed")
    return failures


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("snapshot", "check"))
    parser.add_argument("db")
    parser.add_argument("baseline")
    args = parser.parse_args(argv)
    try:
        if args.mode == "snapshot":
            fd = os.open(args.baseline, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            with os.fdopen(fd, "w") as output:
                json.dump(snapshot(args.db), output, sort_keys=True)
            return 0
        before = json.loads(Path(args.baseline).read_text())
        if before.get("version") != SNAPSHOT_VERSION:
            raise CheckError("table baseline was written by an incompatible version; regenerate it")
        failures = compare(before, snapshot(args.db, before))
        for line in failures:
            print(line)
        return int(bool(failures))
    except CheckError as error:
        print(str(error))
    except (sqlite3.Error, OSError, ValueError, KeyError, TypeError) as error:
        print(f"table digest check could not read the database or baseline: {type(error).__name__}: {error}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
