#!/usr/bin/env python3
"""Check a database adopted into other directories than the ones Jellyfin wrote it in.

A Debian package migration copies /var/lib/jellyfin into /var/lib/ferrofin and /etc/jellyfin into
/etc/ferrofin (scripts/migrate-jellyfin.sh). Jellyfin stores the library folders, artwork and
profile images under those directories as absolute paths, while the ids of the items under program
data derive from the path relative to it, so after the move every id still matches and only the
stored paths are stale. The first start points each such path at the copy under Ferrofin's
directories, where the copy holds the file, and leaves the others alone (docs/INSTALL.md, the boot
repair rebase_program_data_paths). Jellyfin has no such repair, so it cannot be the oracle here:
the expectation is the documented one.

snapshot, before Ferrofin starts: every stored path under a previous directory and what it must
read after the first boot, the item and image type of each such image row, the libraries
(CollectionFolder rows) by name, which of them have a Primary image, and the users with a profile
image. --moved OLD NEW HOST, once per directory, names a previous directory, the one it moved to
(both as the servers see them) and where the copy is on this host.

check, after a boot: every recorded path reads what it must (or, for an image row, a new row of its
item and type replaced it, a file the copy holds under a new directory), and no other row names a
previous directory where the copy holds its file; the libraries are the same rows; no library's
stored Data still names a previous directory (the virtual-folder sync rewrites it at boot);
/Library/VirtualFolders lists every library with its CollectionFolder id; each library's Primary
image and each profile image is served and decodes; the repair's snapshot of the database exists;
and with --snapshots FILE, the database snapshots beside it are recorded on the first check and a
later check fails on any snapshot taken or rewritten since.
"""

import argparse
from collections import Counter
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import sys

from metadata import Api, CheckError, connect, item_id


# The Jellyfin-owned columns that hold an absolute path under program data or configuration
# (rebase_program_data_paths' PATH_COLUMNS, without Ferrofin's own tables).
COLUMNS = (("BaseItems", "Path"), ("BaseItemImageInfos", "Path"), ("Chapters", "ImagePath"),
           ("MediaStreamInfos", "Path"), ("ImageInfos", "Path"))
# What a fixture must exercise, or it tests no move: library folders, artwork, profile images.
REQUIRED = ("BaseItems", "BaseItemImageInfos", "ImageInfos")
COLLECTION_FOLDER = "MediaBrowser.Controller.Entities.CollectionFolder"
# ImageType.Primary.
PRIMARY = 0
# The extension of the snapshot the repair takes before it rewrites (adoption_repairs.rs).
REPAIR_SNAPSHOT = ".pre-program-data-move"
VERSION = 2


def moved_path(value, moves):
    """What stored path `value` must read once a start has seen the move: the same place under the
    new directory when the copy on the host holds the file, else `value`. `moves` is
    [(old, new, host)]; of the old directories that hold `value`, the longest wins.

    This is the documented rule (docs/INSTALL.md), for any path under an old directory. The repair
    maps the old program data's root/, metadata/, data/ and config/ and the profile images, which is
    every place Jellyfin stores a path under them; a row elsewhere under an old directory would fail
    here, and that gap between the documentation and the repair is worth seeing."""
    if not isinstance(value, str):
        return value
    best = None
    for old, new, host in moves:
        old = old.rstrip("/")
        if value.startswith(old + "/") and (best is None or len(old) > len(best[0])):
            best = (old, new.rstrip("/"), host)
    if best is None:
        return value
    old, new, host = best
    rest = value[len(old) + 1:]
    return f"{new}/{rest}" if (Path(host) / rest).exists() else value


def in_copy(value, moves):
    """Whether `value` names a file under one of the new directories that the copy on this host holds."""
    # moved_path read backwards: it maps a path under a new directory only when the copy holds the file.
    return isinstance(value, str) and moved_path(value, [(new, old, host) for old, new, host in moves]) != value


def under(value, moves):
    """Whether `value` names a path under one of the previous directories."""
    return isinstance(value, str) and any(value.startswith(old.rstrip("/") + "/") for old, _, _ in moves)


def normalize(value):
    """A GUID spelled any way as 32 lowercase hex digits (0007 and 0032 change the spelling)."""
    if isinstance(value, str) and len(value) in (32, 36):
        try:
            return item_id(value)
        except ValueError:
            pass
    return value


def keyed(conn, table, column):
    """{primary key: stored path} of every row of `table` whose `column` is set."""
    info = list(conn.execute(f'PRAGMA table_info("{table}")'))
    if not info:
        raise CheckError(f"table {table} is missing")
    keys = [row[1] for row in sorted(info, key=lambda row: row[5]) if row[5]]
    if not keys:
        raise CheckError(f"table {table} has no primary key to name its rows by")
    names = ", ".join(f'"{name}"' for name in keys)
    return {"/".join(str(normalize(part)) for part in row[:-1]): row[-1] for row in conn.execute(
        f'SELECT {names}, "{column}" FROM "{table}" WHERE "{column}" IS NOT NULL')}


def libraries(conn):
    """{normalized id: (name, Data)} of every library (CollectionFolder row)."""
    return {normalize(item): (name, data) for item, name, data in conn.execute(
        'SELECT "Id", "Name", "Data" FROM "BaseItems" WHERE "Type" = ?', (COLLECTION_FOLDER,))}


def snapshot(db, moves):
    """The baseline: see the module docstring."""
    if not moves:
        raise CheckError("a program-data move baseline needs at least one --moved directory")
    with closing(connect(db)) as conn:
        conn.execute("BEGIN")
        paths = {}
        for table, column in COLUMNS:
            paths[table] = {key: [stored, moved_path(stored, moves)]
                            for key, stored in keyed(conn, table, column).items() if under(stored, moves)}
        owners = {str(normalize(key)): [normalize(item), kind] for key, item, kind in conn.execute(
            'SELECT "Id", "ItemId", "ImageType" FROM "BaseItemImageInfos"')}
        found = libraries(conn)
        images = {normalize(item) for (item,) in conn.execute(
            'SELECT "ItemId" FROM "BaseItemImageInfos" WHERE "ImageType" = ?', (PRIMARY,))}
        profiles = sorted({normalize(user) for (user,) in conn.execute(
            'SELECT "UserId" FROM "ImageInfos" WHERE "UserId" IS NOT NULL AND "Path" IS NOT NULL')})
    baseline = {"version": VERSION, "moves": [list(move) for move in moves], "paths": paths,
                "image_owners": {key: owners[key] for key in paths["BaseItemImageInfos"]},
                "libraries": {item: name for item, (name, _) in found.items()},
                "library_images": sorted(item for item in found if item in images), "profiles": profiles}
    validate(baseline)
    return baseline


def validate(baseline):
    """Refuse a baseline that would check no move: a fixture Jellyfin did not write in the layout."""
    if not isinstance(baseline, dict) or baseline.get("version") != VERSION or not baseline.get("moves"):
        raise CheckError("unsupported program-data move baseline; take it again")
    unmoved = [table for table in REQUIRED
               if not any(before != after for before, after in baseline["paths"].get(table, {}).values())]
    if unmoved:
        raise CheckError("the fixture moves no path in " + ", ".join(unmoved)
                         + ": was it built in its layout, and copied whole?")
    if not baseline["libraries"] or not baseline["library_images"] or not baseline["profiles"]:
        raise CheckError("the fixture needs a library, a library image and a profile image")


def first(rows):
    """The first of `rows` in key order, as '<key>: <value>'."""
    key = min(rows)
    return f"{key}: {rows[key]}"


def unreplaced(conn, gone, baseline, moves):
    """The image rows of `gone` that no redraw replaced. A redraw (the scan's library tile, once 7 days
    old) saves the item's images again at new ids, as Jellyfin's every save does, at Ferrofin's own
    path: the rows of an item and image type pass when the item now has as many rows of that type at
    ids the baseline did not record, each a file the copy holds under a new directory."""
    owners = {key: tuple(owner) for key, owner in baseline["image_owners"].items()}
    recorded = baseline["paths"]["BaseItemImageInfos"]
    fresh = Counter((normalize(item), kind) for key, item, kind, path in conn.execute(
        'SELECT "Id", "ItemId", "ImageType", "Path" FROM "BaseItemImageInfos"')
        if str(normalize(key)) not in recorded and in_copy(path, moves))
    lost = Counter(owners[key] for key in gone)
    return {key: stored for key, stored in gone.items() if fresh[owners[key]] < lost[owners[key]]}


def check_paths(conn, baseline, moves):
    """One line per table with a recorded path that does not read what it must, or a row that still
    names a previous directory although the copy holds its file."""
    failures = []
    for table, column in COLUMNS:
        now = keyed(conn, table, column)
        recorded = baseline["paths"].get(table, {})
        gone = {key: stored for key, (stored, _) in recorded.items() if key not in now}
        if table == "BaseItemImageInfos":
            gone = unreplaced(conn, gone, baseline, moves)
        stale = {key: now[key] for key, (stored, after) in recorded.items()
                 if key in now and now[key] != after and now[key] == stored}
        other = {key: now[key] for key, (stored, after) in recorded.items()
                 if key in now and now[key] not in (after, stored)}
        # A row the boot or a scan wrote since, still naming a directory that is gone.
        added = {key: value for key, value in now.items()
                 if key not in recorded and moved_path(value, moves) != value}
        for rows, what in ((stale, "still name a previous directory, though the copy holds the file"),
                           (other, "read neither their place in the copy nor what they were"),
                           (gone, "are gone"),
                           (added, "were written since under a previous directory")):
            if rows:
                failures.append(f"{table}.{column}: {len(rows)} path(s) {what}; first {first(rows)}")
    return failures


def check_libraries(conn, baseline, moves):
    """The library rows are the baseline's, and none of their Data names a previous directory."""
    failures = []
    found = libraries(conn)
    added = sorted(set(found) - set(baseline["libraries"]))
    lost = sorted(set(baseline["libraries"]) - set(found))
    if added or lost:
        failures.append(f"CollectionFolder rows changed: {len(added)} added, {len(lost)} lost "
                        f"(of {len(baseline['libraries'])})")
    stale = sorted(name for name, data in found.values()
                   if data and any(old.rstrip("/") + "/" in data for old, _, _ in moves))
    if stale:
        failures.append(f"CollectionFolder Data still names a previous directory: {len(stale)} "
                        f"library(ies), first {stale[0]}")
    return failures


def check_api(api, baseline, moves):
    """/Library/VirtualFolders lists the libraries by their ids, and the artwork is served."""
    failures = []
    folders = api.request("/Library/VirtualFolders")
    if not isinstance(folders, list) or any(not isinstance(row, dict) for row in folders):
        raise CheckError("/Library/VirtualFolders returned an invalid list")
    listed = {row.get("Name"): normalize(row.get("ItemId") or "") for row in folders}
    expected = {name: item for item, name in baseline["libraries"].items()}
    wrong = sorted(name for name, item in listed.items() if expected.get(name) != item)
    missing = sorted(set(expected) - set(listed))
    if wrong or missing:
        failures.append(f"/Library/VirtualFolders: {len(wrong)} listed with another id than their "
                        f"CollectionFolder row, {len(missing)} not listed; first {(wrong + missing)[0]}")
    stale = sorted(row.get("Name") or "" for row in folders
                   if any(under(location, moves) for location in row.get("Locations") or []))
    if stale:
        failures.append(f"/Library/VirtualFolders: {len(stale)} library(ies) list a location under a "
                        f"previous directory, first {stale[0]}")
    for paths, what in ((["/Items/%s/Images/Primary" % item for item in baseline["library_images"]],
                         "library Primary image"),
                        (["/Users/%s/Images/Primary" % user for user in baseline["profiles"]],
                         "profile image")):
        bad = []
        for path in paths:
            try:
                if not api.request(path, image=True):
                    bad.append(f"{path} (empty or undecodable)")
            except CheckError as error:
                bad.append(f"{path} ({error})")
        if bad:
            failures.append(f"{len(bad)} of {len(paths)} {what}(s) not served, first {bad[0]}")
    return failures


def snapshots(db):
    """{name: [size, mtime_ns]} of the database snapshots beside `db` (`jellyfin.db.pre-*`)."""
    db = Path(db)
    return {path.name: [path.stat().st_size, path.stat().st_mtime_ns]
            for path in sorted(db.parent.glob(db.name + ".pre-*")) if not path.name.endswith(".partial")}


def check_snapshots(db, baseline, record):
    """The first check records the snapshots and needs the repair's; a later one fails on any change."""
    now = snapshots(db)
    if not os.path.exists(record):
        fd = os.open(record, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as output:
            json.dump(now, output, sort_keys=True)
        moved = any(before != after for table in baseline["paths"].values() for before, after in table.values())
        name = Path(db).name + REPAIR_SNAPSHOT
        return [f"no {name} beside the database: the paths were not rewritten after a snapshot"] \
            if moved and name not in now else []
    before = json.loads(Path(record).read_text())
    taken = sorted(set(now) - set(before))
    changed = sorted(name for name in set(now) & set(before) if now[name] != before[name])
    lost = sorted(set(before) - set(now))
    failures = []
    if taken:
        failures.append(f"a later boot took {len(taken)} database snapshot(s), first {taken[0]}")
    if changed:
        failures.append(f"{len(changed)} database snapshot(s) rewritten since the first boot, first {changed[0]}")
    if lost:
        failures.append(f"{len(lost)} database snapshot(s) removed since the first boot, first {lost[0]}")
    return failures


def check(db, baseline, base_url, record=None):
    """Every failure of the module docstring's checks, one line each."""
    validate(baseline)
    moves = [tuple(move) for move in baseline["moves"]]
    with closing(connect(db)) as conn:
        conn.execute("BEGIN")
        failures = check_paths(conn, baseline, moves)
        failures += check_libraries(conn, baseline, moves)
    failures += check_api(Api(base_url, db), baseline, moves)
    if record:
        failures += check_snapshots(db, baseline, record)
    return failures


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("mode", choices=("snapshot", "check"))
    parser.add_argument("db")
    parser.add_argument("baseline")
    parser.add_argument("--moved", nargs=3, action="append", default=[], metavar=("OLD", "NEW", "HOST"),
                        help="snapshot: a previous directory, the one it moved to, and the copy on this host")
    parser.add_argument("--base-url", help="check: the server")
    parser.add_argument("--snapshots", help="check: where the first check records the database snapshots")
    args = parser.parse_args(argv)
    try:
        if args.mode == "snapshot":
            data = snapshot(args.db, [tuple(move) for move in args.moved])
            fd = os.open(args.baseline, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            with os.fdopen(fd, "w") as output:
                json.dump(data, output, sort_keys=True)
            return 0
        if not args.base_url:
            raise CheckError("--base-url is required for program-data move checks")
        failures = check(args.db, json.loads(Path(args.baseline).read_text()), args.base_url, args.snapshots)
        for line in failures:
            print(line)
        return int(bool(failures))
    except CheckError as error:
        print(str(error))
    except (sqlite3.Error, OSError, ValueError, KeyError, TypeError) as error:
        print(f"program-data move check could not read the database, baseline or API: {type(error).__name__}: {error}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
