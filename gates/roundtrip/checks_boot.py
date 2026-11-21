#!/usr/bin/env python3
"""Jellyfin's boot on a round trip's export, read from its console log (stdlib only).

    checks_boot.py migrations LOG      a line per migration Jellyfin began and never finished, with the error it
                                       died of (the SQLite error when there is one)
    checks_boot.py errors LOG          ERR and FTL entries outside ALLOWED: how many, and the first few
    checks_boot.py ran LOG ORACLE DB   the schema migrations Jellyfin began (the exported history should record
                                       every one), and the code migrations it ran that differ from those ORACLE,
                                       the log of its own first start on the route's source, shows it running,
                                       less those the source's history (the database DB) records

Exit status 0 when there is nothing to say, 1 with the reasons on stdout, 2 when a log cannot be read or
holds no entry at all (an empty capture, or a format this no longer parses, must never pass), or DB's
history cannot be read.
The log is the container's output: entries such as `[07:53:39] [INF] [10] Main: Jellyfin version: 12.4.0`,
a multi-line entry continuing on the lines that do not start one. The time has milliseconds
(`[07:53:39.252]`) where the configuration's logging.default.json asks for them, as the one a fresh install of
12.2 or later writes does.
"""

from contextlib import closing
from pathlib import Path
import re
import sqlite3
import sys

ENTRY = re.compile(r"^\[\d\d:\d\d:\d\d(?:\.\d+)?\] \[(VRB|DBG|INF|WRN|ERR|FTL)\] \[\d+\] ([^:]+): ?(.*)$")
BEGUN = re.compile(r"^Perform migration (\S+)$")
DONE = re.compile(r"^Migration (\S+) was successfully applied$")
SQLITE = re.compile(r"SQLite Error \d+: '[^\n]*?'")
#: The errors a round trip expects, each with why: (pattern on "<source>: <first line>", reason). Only what a
#: live run has logged is here: a provider call behind the proxy gets an entry once one is seen, not before.
ALLOWED = (
    (re.compile(r"^Emby\.Server\.Implementations\.Updates\.InstallationManager: An error occurred while accessing "
                r"the plugin manifest"), "the plugin repository, behind the unreachable proxy"),
)
#: How many unexpected errors a reason quotes before it only counts the rest.
MAX_SHOWN = 3


def entries(text):
    """[(level, source, first line, whole text)] in log order."""
    out = []
    for line in text.splitlines():
        match = ENTRY.match(line)
        if match:
            out.append([match[1], match[2], match[3], match[3]])
        elif out:
            out[-1][3] += "\n" + line
    return [tuple(entry) for entry in out]


def migrations(log):
    found, begun = entries(log), {}
    for index, (_, _, first, _) in enumerate(found):
        if match := BEGUN.match(first):
            begun[match[1]] = index
        elif match := DONE.match(first):
            begun.pop(match[1], None)
    reasons = []
    for migration, index in begun.items():
        after = [entry for entry in found[index + 1:] if entry[0] in ("ERR", "FTL")]
        sqlite = next((match[0] for entry in after if (match := SQLITE.search(entry[3]))), None)
        cause = sqlite or (f"{after[0][1]}: {after[0][2]}" if after else "no error logged after it began")
        reasons.append(f"migration {migration} failed: {cause}")
    return reasons


def performed(log):
    """(code, schema): the code and the schema (EF) migrations Jellyfin began, in log order."""
    code, schema = [], []
    for _, source, first, _ in entries(log):
        if match := BEGUN.match(first):
            {"InternalCodeMigration": code, "InternalDatabaseMigration": schema}.get(source, code).append(match[1])
    return code, schema


def recorded(path):
    """The ids the `__EFMigrationsHistory` of the database at path records."""
    with closing(sqlite3.connect(Path(path).resolve().as_uri() + "?mode=ro", uri=True)) as db:
        return {row[0] for row in db.execute('SELECT "MigrationId" FROM "__EFMigrationsHistory"')}


def ran(log, oracle, source=frozenset()):
    code, schema = performed(log)
    # A routine the source records ran there only because a schema migration applied before it reverted the one
    # whose Down() drops its row (on 10.11.10 and 10.11.11, AddNormalizedUsername's drops UpdateNormalizedUsername's,
    # JellyfinMigrationService.MigrateStepAsync): the export applies no schema migration, so it stays recorded.
    expected = [m for m in performed(oracle)[0] if m not in source]
    reasons = []
    if schema:
        reasons.append(f"it began {len(schema)} schema migration(s) the history should have recorded: "
                       f"{shown(schema)}")
    if missing := [m for m in expected if m not in code]:
        reasons.append(f"{len(missing)} code migration(s) it runs on the source did not run: {shown(missing)}")
    if extra := [m for m in code if m not in expected]:
        reasons.append(f"{len(extra)} code migration(s) ran that it does not run on the source: {shown(extra)}")
    return reasons


def shown(names):
    more = f"; and {len(names) - MAX_SHOWN} more" if len(names) > MAX_SHOWN else ""
    return ", ".join(names[:MAX_SHOWN]) + more


def errors(log):
    unexpected = [f"[{level}] {source}: {first}" for level, source, first, _ in entries(log)
                  if level in ("ERR", "FTL") and not any(p.match(f"{source}: {first}") for p, _ in ALLOWED)]
    if not unexpected:
        return []
    more = f"; and {len(unexpected) - MAX_SHOWN} more" if len(unexpected) > MAX_SHOWN else ""
    return [f"{len(unexpected)} unexpected error(s): {'; '.join(e[:200] for e in unexpected[:MAX_SHOWN])}{more}"]


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if not argv or len(argv) != {"migrations": 2, "errors": 2, "ran": 4}.get(argv[0], 0):
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2
    logs = []
    for path in argv[1:3]:
        try:
            with open(path, encoding="utf-8", errors="replace") as handle:
                logs.append(handle.read())
        except OSError as error:
            print(f"cannot read the Jellyfin log: {error}")
            return 2
        if not entries(logs[-1]):
            print(f"no Jellyfin log entry in {path}")
            return 2
    if argv[0] == "ran":
        try:
            logs.append(recorded(argv[3]))
        except sqlite3.Error as error:
            print(f"cannot read the source's history in {argv[3]}: {error}")
            return 2
    reasons = {"migrations": migrations, "errors": errors, "ran": ran}[argv[0]](*logs)
    for reason in reasons:
        print(reason)
    return int(bool(reasons))


if __name__ == "__main__":
    sys.exit(main())
