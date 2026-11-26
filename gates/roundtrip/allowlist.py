#!/usr/bin/env python3
"""What Jellyfin may change in a round trip's database without failing the row diff (checks_db.py rows), each
entry with its cause (stdlib only).

Jellyfin runs on the export in windows, and checks_db.py compares a copy of its database taken at the end of each
window with the one taken at its start:

    start      the export as written, to Jellyfin started and its startup tasks finished
    capture    to the gate's playlist refresh, answers, Live TV check (with its guide refresh) and probes done
               (the gate's own requests)
    scan       to its library scan completed and its queued work finished
    stop       to Jellyfin stopped: after the scan, the gate's answers again; on the scale route, all it did after
               its start
    restart    to Jellyfin started a second time and its startup tasks finished

Every row a window added, removed or changed must be explained by an entry, or the window's check fails and
names it. An entry is a Rule: its name, the windows it applies in, the table, the change (added, removed or changed), for
a changed row the columns it may change (within a relative tolerance, for a number; or, for a JSON object such as
BaseItems.Data, only in the keys it names), the rows it applies to
(`where`: (column, a regular expression the whole value must match) pairs, read from the row before a change or
removal and after an addition), its kind and its cause:

    timestamp    a stamp or a key Jellyfin writes anew whenever it saves or refreshes a row, whatever changed
    session      what signing in or presenting a token writes
    gate         what the gate's own requests write
    checked      a change another of the gate's checks judges one by one
    allowlisted  a difference the owner decided to accept, or an audit verdict (gl-1573 adds these), its cause
                 naming the issue that decided it (gl-<iid>)

A changed row is explained when every column it changed is covered by an entry that applies to it. The entries
are checked before a run (`allowlist.py check --schema FILE`, run.sh and the gate's tests): each has a name of
its own, names a table and columns Jellyfin's schema has, a window, a change and a kind this file knows, and a
cause (an allowlisted one citing its issue); a tolerance only on a numeric column, JSON keys only on a text one. Exit status 0 when every entry holds; 1 with a line per problem.
"""

import argparse
from contextlib import closing
from dataclasses import dataclass
import json
import re
import sqlite3
import sys

WINDOWS = ("start", "capture", "scan", "stop", "restart")
CHANGES = ("added", "removed", "changed")
KINDS = ("timestamp", "session", "gate", "checked", "allowlisted")
#: The windows the gate's own requests run in: its playlist refresh, answers, Live TV check and probes, and its
#: answers again after the scan.
GATE_WINDOWS = ("capture", "stop")
#: The windows of Jellyfin's first start, when it saves and refreshes items; a second start must touch none.
FIRST_RUN = ("start", "capture", "scan", "stop")


@dataclass(frozen=True)
class Rule:
    """One allowlist entry (the module's docstring); `name` names it in the row diff's files, so it stays the same
    when entries are added or moved."""

    name: str
    windows: tuple
    table: str
    change: str
    kind: str
    cause: str
    columns: tuple = ()
    where: tuple = ()
    within: float = None
    keys: tuple = ()

    def applies(self, window, table, change, row):
        """Whether the entry applies to a change of this kind to `row` ({column: value}) in this window."""
        return (window in self.windows and table == self.table and change == self.change
                and all(re.fullmatch(pattern, str(row.get(column))) for column, pattern in self.where))

    def covers(self, column, before, after):
        """Whether the entry lets `column` go from `before` to `after`."""
        if column not in self.columns:
            return False
        if self.keys:   # some named key changed, and only named keys: a mere rewrite is no stamp's doing
            old, new = json_object(before), json_object(after)
            if old is None or new is None:
                return False
            moved = {k for k in set(old) | set(new) if old.get(k, MISSING) != new.get(k, MISSING)}
            return bool(moved) and moved <= set(self.keys)
        if self.within is None:
            return True
        if not all(isinstance(v, (int, float)) for v in (before, after)):
            return False
        return abs(after - before) <= self.within * abs(before)


#: A JSON key one side of a change lacks.
MISSING = object()


def json_object(raw):
    """A stored JSON object (BaseItems.Data) as a dict; None for anything else."""
    try:
        found = json.loads(raw) if isinstance(raw, str) else None
    except ValueError:
        return None
    return found if isinstance(found, dict) else None


RULES = (
    Rule("saved", FIRST_RUN, "BaseItems", "changed", "timestamp", columns=("DateLastSaved",),
         cause="LibraryManager.UpdateItemsAsync stamps DateLastSaved on every save, whatever changed "
               "(Emby.Server.Implementations/Library/LibraryManager.cs:2667,2671), as ProviderManager.SaveMetadataAsync "
               "does after a metadata saver wrote (MediaBrowser.Providers/Manager/ProviderManager.cs:833,849). It covers "
               "guide programmes too, where the owner accepted that Jellyfin saves every one on each guide refresh "
               "and Ferrofin only a changed one (gl-1628)"),
    Rule("saved-in-data", FIRST_RUN, "BaseItems", "changed", "timestamp", columns=("Data",), keys=("DateLastSaved",),
         cause="Data is the whole item serialized (Jellyfin.Server.Implementations/Item/BaseItemMapper.cs:268), its "
               "DateLastSaved with it, which every save stamps as above"),
    Rule("refreshed", FIRST_RUN, "BaseItems", "changed", "timestamp", columns=("DateLastRefreshed",),
         cause="MetadataService.RefreshMetadata stamps DateLastRefreshed on every refresh whose providers ran "
               "(MediaBrowser.Providers/Manager/MetadataService.cs:223)"),
    Rule("image-id", FIRST_RUN, "BaseItemImageInfos", "changed", "timestamp", columns=("Id",),
         cause="every save of an item gives each of its image rows a new Id (Jellyfin.Server.Implementations/Item/"
               "BaseItemMapper.cs:455, MapImageToEntity); checks_db.py pairs the rows by item, type and path"),
    Rule("gate-sign-in", GATE_WINDOWS, "Users", "changed", "gate", columns=("LastLoginDate", "LastActivityDate", "RowVersion"),
         where=(("Username", "roundtrip-(user|passwordless)"),),
         cause="answers.py signs in each account the usage stage made (GATE_USERS), and a sign-in stamps both dates "
               "(Jellyfin.Server.Implementations/Users/UserManager.cs:657-662); every save of a user counts up its "
               "RowVersion (src/Jellyfin.Database/Jellyfin.Database.Implementations/Entities/User.cs:343-345, called from "
               "JellyfinDbContext.cs:314-322)"),
    Rule("user-activity", GATE_WINDOWS, "Users", "changed", "session", columns=("LastActivityDate", "RowVersion"),
         cause="a request with an account's token stamps the account's LastActivityDate, at most once a minute, and "
               "saves it, which counts up its RowVersion (Emby.Server.Implementations/Session/SessionManager.cs:263-273, "
               "User.cs:343-345): smoke.sh probes through the probed account's own web session"),
    Rule("device-activity", GATE_WINDOWS, "Devices", "changed", "session",
         columns=("DateLastActivity", "AppVersion", "DeviceName"),
         cause="a request with a device's token stamps its DateLastActivity when the last is over 3 minutes old, and "
               "takes the request's app version and device name, then saves the row (Jellyfin.Server.Implementations/"
               "Security/AuthorizationContext.cs:158-188, Devices/DeviceManager.cs:224-229): smoke.sh presents a web "
               "session's token with its own client headers"),
    Rule("gate-device-gone", GATE_WINDOWS, "Devices", "removed", "gate", where=(("DeviceId", "roundtrip-.*"),),
         cause="answers.py signs each account in GATE_USERS in from its own device (answers.DEVICE_ID), and a sign-in "
               "logs out that device's earlier tokens, deleting their rows (Emby.Server.Implementations/Session/"
               "SessionManager.cs:1789-1802)"),
    Rule("gate-device-new", GATE_WINDOWS, "Devices", "added", "gate", where=(("DeviceId", "roundtrip-.*"),),
         cause="each of answers.py's sign-ins creates its device row (Emby.Server.Implementations/Session/"
               "SessionManager.cs:1810)"),
    Rule("gate-sign-in-log", GATE_WINDOWS, "ActivityLogs", "added", "gate",
         where=(("Type", "AuthenticationSucceeded|AuthenticationFailed"), ("Name", r".*\broundtrip-(user|passwordless)\b.*")),
         cause="Jellyfin logs each sign-in, failed or not (Jellyfin.Server.Implementations/Events/Consumers/Security/"
               "Authentication{Succeeded,Failed}Logger.cs): answers.py signs in each account in GATE_USERS, the one "
               "with a password first with a wrong one"),
    Rule("session-log", GATE_WINDOWS, "ActivityLogs", "added", "session", where=(("Type", "SessionStarted|SessionEnded"),),
         cause="Jellyfin logs each session it starts or ends (Jellyfin.Server.Implementations/Events/Consumers/Session/"
               "Session{Started,Ended}Logger.cs); a request from a client and device it holds no session for starts "
               "one (Emby.Server.Implementations/Session/SessionManager.cs:500-511): the answers' sign-ins and "
               "smoke.sh's requests"),
    Rule("trickplay-bandwidth", WINDOWS, "TrickplayInfos", "changed", "allowlisted", columns=("Bandwidth",), within=0.05,
         cause="the owner accepted a trickplay Bandwidth within 5% of Ferrofin's: the tiles come from another JPEG "
               "encoder (gl-1596)"),
    Rule("migration-history", ("start",), "__EFMigrationsHistory", "added", "checked",
         cause="Jellyfin records each code migration it runs (Jellyfin.Server/Migrations/JellyfinMigrationService.cs:"
               "526-528), as EF does each schema migration; 'it ran the code migrations it runs on the source, and no "
               "schema migration' checks them one by one"),
)


def schema_columns(path):
    """{table: {column: declared type}} of the schema SQL at `path` (Jellyfin's pinned schema)."""
    with open(path, encoding="utf-8") as handle:
        sql = handle.read()
    with closing(sqlite3.connect(":memory:")) as db:
        db.executescript(sql)
        tables = [name for (name,) in db.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")]
        return {table: {row[1]: (row[2] or "").upper() for row in db.execute(f'PRAGMA table_info("{table}")')}
                for table in tables}


def problems(rules, schema):
    """A line per entry of `rules` that does not hold against `schema` (schema_columns)."""
    out = []
    for number, rule in enumerate(rules, 1):
        name = f"entry {number} ({rule.name or '?'}: {rule.table} {rule.change})"
        if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", rule.name):
            out.append(f"{name}: a name is lower-case letters, digits and dashes")
        columns = schema.get(rule.table)
        if columns is None:
            out.append(f"{name}: no table {rule.table} in Jellyfin's schema")
            continue
        if not rule.windows or set(rule.windows) - set(WINDOWS):
            out.append(f"{name}: windows {list(rule.windows)} not among {', '.join(WINDOWS)}")
        if rule.change not in CHANGES:
            out.append(f"{name}: change {rule.change!r} not among {', '.join(CHANGES)}")
        if rule.kind not in KINDS:
            out.append(f"{name}: kind {rule.kind!r} not among {', '.join(KINDS)}")
        if not rule.cause.strip():
            out.append(f"{name}: no cause")
        elif rule.kind == "allowlisted" and not re.search(r"\bgl-\d+\b", rule.cause):
            out.append(f"{name}: an allowlisted entry's cause names the issue that decided it (gl-<iid>)")
        if (rule.change == "changed") != bool(rule.columns):
            out.append(f"{name}: a changed row needs the columns it may change, and only a changed row has them")
        for column in (*rule.columns, *(column for column, _ in rule.where)):
            if column not in columns:
                out.append(f"{name}: no column {column} in {rule.table}")
        for column, pattern in rule.where:
            try:
                re.compile(pattern)
            except re.error as error:
                out.append(f"{name}: where {column}: {error}")
        if rule.keys and (rule.within is not None or any(columns.get(c) != "TEXT" for c in rule.columns)):
            out.append(f"{name}: JSON keys only on text columns, and not with a tolerance")
        if rule.within is not None:
            if not 0 < rule.within < 1:
                out.append(f"{name}: a tolerance is a fraction between 0 and 1, not {rule.within}")
            textual = [c for c in rule.columns if c in columns and columns[c] not in ("INTEGER", "REAL")]
            if textual:
                out.append(f"{name}: a tolerance on a column that is not a number: {', '.join(textual)}")
    if len(set(rules)) != len(rules):
        out.append("an entry is listed twice")
    if len({rule.name for rule in rules}) != len(rules):
        out.append("two entries share a name")
    return out


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("mode", choices=("check",))
    parser.add_argument("--schema", required=True, help="Jellyfin's pinned schema SQL")
    args = parser.parse_args(argv)
    try:
        found = problems(RULES, schema_columns(args.schema))
    except (OSError, sqlite3.Error) as error:
        found = [f"cannot read Jellyfin's schema {args.schema}: {error}"]
    for problem in found:
        print(f"allowlist: {problem}")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
