#!/usr/bin/env python3
"""A 40,000-item scale fixture, and what adopting it costs.

    scale.py build --fixtures DIR [--items N]   derive the scale fixture from a finished fixture by
                                                cloning its media items with SQL
    scale.py need-disk FIXTURE WORK             exit 1, saying why, unless WORK has the free disk a
                                                run of FIXTURE needs
    scale.py counts DB OUT                      the row counts adoption must not lower (a baseline)
    scale.py check DB BASELINE                  every cloned item and its rows survived
    scale.py sample CONTAINER DIR OUT           sample disk and memory until SIGTERM, then write OUT
    scale.py record LOG SAMPLES DB OUT          migration time, memory, disk and foreign_key_check
                                                cost of one adoption, as one line and as OUT

Needs Python 3.11 or newer (RFC 3339 stamps with nanoseconds, sqlite3 limits).

Cloning: the library of a scanned synthetic fixture is repeated, item by item, until the database
holds ITEMS rows of BaseItems, so no 40,000 media files are needed. A clone is a media item (a movie,
an episode, a song) with a new id, a path of its own (in the tree the scale row mounts, SCALE_MEDIA,
a hard link to a template file of the same name and grandparent folder), and a copy of every row
that hangs off its template: streams, chapters, attachments, trickplay, segments, images, providers,
metadata fields, ancestors, item values, people and, for one clone in USERDATA_EVERY, the play state
of every user the fixture holds play state for. Item values and people are shared, as they are in a
real library. Scan cost at scale belongs to the benchmark corpus, so nothing here scans.

Measuring (recorded, not gated: thresholds come from the first runs): the container's cgroup keeps
its own memory peak, which counts the page cache of the files the boot reads and writes, so the
largest anonymous memory seen by the sampler is recorded beside it as the closer match for resident
memory. Disk is sampled every SAMPLE_S during the boot (the database, its WAL and the .pre-* snapshots
beside it), so a short peak between two samples is missed and the numbers are lower bounds. The
migration time is read off the boot log, and the cost of the every-boot foreign_key_check is the
time of the same pragma on the adopted file. Stdlib plus the harness's own modules.
"""

import argparse
from contextlib import closing
from datetime import datetime
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
import uuid

from messy import spelled
from metadata import CheckError
from synthetic_fixture import seed_id

#: The fixture the scale fixture is derived from, and its name. The 10.11 lineage runs the heavy
#: migrations (0007, 0032 rebuild BaseItems and its neighbours), so it is the one to time.
DERIVED = ("jellyfin-10.11.11-scale", "jellyfin-10.11.11")
#: Items in BaseItems once the clones are in.
ITEMS = 40000
#: One clone in this many carries play state, which gives about 4,000 items with state per user
#: at ITEMS: a heavy household, enough rows to make UserData matter.
USERDATA_EVERY = 10
#: Seconds between samples while the container boots: the size of the data directory and its WAL,
#: the cgroup's memory peak and its anonymous memory (the cgroup keeps the peak itself, but not the
#: anonymous peak, which is the largest value seen).
SAMPLE_S = 1
#: Free disk a run needs, as a multiple of the database it adopts: the working copy, the
#: .pre-ferrofin and .pre-00NN snapshots, the WAL, and the room a table rebuild takes. The first
#: measured run's whole data directory reached 5.7 times the database, so the floor (6 times the
#: database plus the rest of the fixture) is only about 0.3 of the database above it.
FREE_MULTIPLE = 6
#: A media item worth repeating: these types, with a file of their own.
TEMPLATE_TYPES = ("MediaBrowser.Controller.Entities.Movies.Movie", "MediaBrowser.Controller.Entities.TV.Episode",
                  "MediaBrowser.Controller.Entities.Audio.Audio")
#: Tables whose rows hang off an item through one column, and the GUID column to give every
#: copy a new value (None when the item column is part of the key). Every table with a foreign key to
#: BaseItems must be here (or be UserData, which has its own rule), so a table added later fails the
#: build instead of going uncloned.
CHILDREN = {
    "AncestorIds": ("ItemId", None),
    "AttachmentStreamInfos": ("ItemId", None),
    "BaseItemImageInfos": ("ItemId", "Id"),
    "BaseItemMetadataFields": ("ItemId", None),
    "BaseItemProviders": ("ItemId", None),
    "BaseItemTrailerTypes": ("ItemId", None),
    "Chapters": ("ItemId", None),
    "ItemValuesMap": ("ItemId", None),
    "KeyframeData": ("ItemId", None),
    "MediaSegments": ("ItemId", "Id"),
    "MediaStreamInfos": ("ItemId", None),
    "PeopleBaseItemMap": ("ItemId", None),
    "TrickplayInfos": ("ItemId", None),
}
#: What the check compares: tables of rows that hang off a clone, and BaseItems itself.
CLONE_MARK = "%/scale-%/%"


# ── cloning ─────────────────────────────────────────────────────────────────
def tables_of(conn):
    return {name for (name,) in conn.execute("SELECT name FROM sqlite_master WHERE type = 'table'")}


def referencing(conn):
    """Tables with a foreign key to BaseItems.Id."""
    return {name for name in tables_of(conn)
            if any(row[2] == "BaseItems" for row in conn.execute(f'PRAGMA foreign_key_list("{name}")'))}


def violations(conn):
    return len(conn.execute("PRAGMA foreign_key_check").fetchall())


def columns_of(conn, table):
    return [row[1] for row in conn.execute(f'PRAGMA table_info("{table}")')]


def templates(conn):
    """The media items to repeat: no owner (a 10.11 database writes the all-zero Guid for none, a
    12.x one NULL), no primary version, not an extra, with a path."""
    marks = ", ".join("?" * len(TEMPLATE_TYPES))
    cursor = conn.execute(
        f'SELECT * FROM "BaseItems" WHERE "Type" IN ({marks}) AND "Path" IS NOT NULL '
        "AND (\"OwnerId\" IS NULL OR replace(\"OwnerId\", '-', '') = '00000000000000000000000000000000') "
        'AND "PrimaryVersionId" IS NULL AND ("ExtraType" IS NULL OR "ExtraType" = 0) ORDER BY "Id"', TEMPLATE_TYPES)
    names = [column[0] for column in cursor.description]
    return [dict(zip(names, row)) for row in cursor.fetchall()]


#: The media tree the scale row mounts as /media: a hard-linked copy of the fixtures' own, plus, at
#: each clone's path, a link to its template's file. A 10.11 database's adoption removes an item whose
#: file is gone under a readable library location (Jellyfin 12's MigrateLinkedChildren, and Ferrofin's
#: port of it), so every clone needs a file; hard links cost no data.
SCALE_MEDIA = "media-scale"


def link_clone_files(root, clone_paths, template_paths):
    """Build `root`/SCALE_MEDIA from `root`/media: each clone path (a container path under /media) a
    hard link to a template file with its grandparent folder and file name (clone_path keeps both;
    which template, when two share them, does not matter: the sweep only asks that a file is there).
    Built beside and renamed into place, so a partial tree is never mounted."""
    by_place = {(os.path.dirname(os.path.dirname(path)), os.path.basename(path)): path for path in template_paths}
    tree, partial = root / SCALE_MEDIA, root / (SCALE_MEDIA + ".partial")
    for leftover in (tree, partial):
        shutil.rmtree(leftover, ignore_errors=True)
    shutil.copytree(root / "media", partial, copy_function=os.link)
    for path in clone_paths:
        template = by_place.get((os.path.dirname(os.path.dirname(path)), os.path.basename(path)))
        if template is None or not (path.startswith("/media/") and template.startswith("/media/")):
            raise CheckError(f"scale media: no template file under /media/ for the clone {path}")
        source, target = partial / template.removeprefix("/media/"), partial / path.removeprefix("/media/")
        if not source.is_file():
            raise CheckError(f"scale media: the template file {template} is not in {root / 'media'}")
        target.parent.mkdir(parents=True, exist_ok=True)
        os.link(source, target)
    partial.rename(tree)


def clone_path(path, number):
    """A path in a directory of its own beside the template's, which no file has."""
    folder, name = os.path.split(path)
    return f"{os.path.dirname(folder)}/scale-{number:05d}/{name}"


def item_row(template, number, names):
    ident = seed_id(f"scale-{number}")
    row = dict(template)
    row["Id"] = spelled(template["Id"], ident)
    row["Path"] = clone_path(template["Path"], number)
    for column in ("Name", "SortName", "CleanName"):
        if row.get(column) is not None:
            row[column] = f"{row[column]} {number:05d}"
    row["PresentationUniqueKey"] = ident.hex
    return ident, row["Id"], tuple(row[name] for name in names)


def insert(conn, table, names, rows):
    columns = ", ".join(f'"{name}"' for name in names)
    conn.executemany(f'INSERT INTO "{table}" ({columns}) VALUES ({", ".join("?" * len(names))})', rows)


def child_rows(conn, table, column, wanted):
    """(columns, {template id: [row dict]}) of the rows of `table` hanging off any of `wanted`."""
    names = columns_of(conn, table)
    marks = ", ".join("?" * len(wanted))
    by_item = {}
    for record in conn.execute(f'SELECT * FROM "{table}" WHERE "{column}" IN ({marks})', wanted):
        by_item.setdefault(record[names.index(column)], []).append(dict(zip(names, record)))
    return names, by_item


def userdata_shape(conn):
    """The UserData row of each user to copy for a template that has none of its own."""
    shape = {}
    for record in conn.execute('SELECT * FROM "UserData" ORDER BY "ItemId", "CustomDataKey"'):
        row = dict(zip(columns_of(conn, "UserData"), record))
        shape.setdefault(row["UserId"], row)
    return shape


def clone_items(conn, items):
    """Grow BaseItems to `items` rows by repeating the templates round-robin; returns rows added
    per table. Foreign keys are checked before and after, so a clone can add no violation."""
    unknown = referencing(conn) - set(CHILDREN) - {"BaseItems", "UserData"}
    if unknown:
        raise CheckError("scale.py does not clone the rows of " + ", ".join(sorted(unknown)) + "; add them to CHILDREN")
    existing = conn.execute('SELECT COUNT(*) FROM "BaseItems"').fetchone()[0]
    if existing >= items:
        raise CheckError(f"the fixture already holds {existing} items, not fewer than the {items} asked for")
    sources = templates(conn)
    if not sources:
        raise CheckError("the fixture has no movie, episode or song to repeat")
    before = violations(conn)
    wanted = [source["Id"] for source in sources]
    if len(wanted) > conn.getlimit(sqlite3.SQLITE_LIMIT_VARIABLE_NUMBER):
        raise CheckError(f"the fixture holds {len(wanted)} items to repeat, more than one query can name; "
                         "derive the scale fixture from a smaller library")
    names = columns_of(conn, "BaseItems")
    added = {"BaseItems": 0, "UserData": 0}
    clones = [(number, sources[(number - 1) % len(sources)]) for number in range(1, items - existing + 1)]
    rows = []
    ids = {}
    for number, source in clones:
        ident, stored, row = item_row(source, number, names)
        ids[number] = (ident, stored)
        rows.append(row)
    insert(conn, "BaseItems", names, rows)
    added["BaseItems"] = len(rows)
    for table, (column, guid) in CHILDREN.items():
        if table not in tables_of(conn):
            continue
        table_names, by_item = child_rows(conn, table, column, wanted)

        def copies(table=table, column=column, guid=guid, by_item=by_item, table_names=table_names):
            for number, source in clones:
                ident, stored = ids[number]
                for index, row in enumerate(by_item.get(source["Id"], ())):
                    row = dict(row)
                    row[column] = stored
                    if guid:
                        row[guid] = spelled(row[guid], seed_id(f"scale-{number}-{table}-{index}"))
                    yield tuple(row[name] for name in table_names)

        before_rows = conn.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0]
        insert(conn, table, table_names, copies())
        added[table] = conn.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0] - before_rows
    # play state: the template's own row for each user, else the fixture's first row for that user
    table_names, by_item = child_rows(conn, "UserData", "ItemId", wanted)
    fallback = userdata_shape(conn)
    if not fallback:
        raise CheckError("the fixture has no play state to copy")

    def play_state():
        for number, source in clones:
            if number % USERDATA_EVERY:
                continue
            ident, stored = ids[number]
            own = {row["UserId"]: row for row in by_item.get(source["Id"], ())}
            for user, shape in fallback.items():
                row = dict(own.get(user, shape))
                row["ItemId"] = stored
                row["CustomDataKey"] = ident.hex
                yield tuple(row[name] for name in table_names)

    before_rows = conn.execute('SELECT COUNT(*) FROM "UserData"').fetchone()[0]
    insert(conn, "UserData", table_names, play_state())
    added["UserData"] = conn.execute('SELECT COUNT(*) FROM "UserData"').fetchone()[0] - before_rows
    if violations(conn) != before:
        raise CheckError("the clones added foreign key violations")
    return added


def save(path, data):
    with open(path, "w") as output:
        os.fchmod(output.fileno(), 0o600)
        json.dump(data, output, indent=2, sort_keys=True)


def build(root, items=ITEMS):
    name, source_name = DERIVED
    target, source = root / name, root / source_name
    if (target / "scale.json").exists():
        print(name + " already built", flush=True)
        return
    if not (source / "synthetic-ready.json").exists():
        raise CheckError(f"{source_name} is not built; run build-fixtures.sh first")
    if target.exists():
        raise CheckError(f"{target} is incomplete; remove it (it is a derived copy) and build again")
    try:
        shutil.copytree(source, target)
        db = target / "data/jellyfin.db"
        with closing(sqlite3.connect(db)) as conn:
            template_paths = [template["Path"] for template in templates(conn)]
            conn.execute("PRAGMA foreign_keys = OFF")
            with conn:
                added = clone_items(conn, items)
            conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
            clone_paths = [path for (path,) in conn.execute(
                f'SELECT "Path" FROM "BaseItems" WHERE "Path" LIKE \'{CLONE_MARK}\'')]
        link_clone_files(root, clone_paths, template_paths)
        save(target / "scale.json", {"version": 1, "source": source_name, "items": items, "added": added,
                                     "userdata_every": USERDATA_EVERY, "database_bytes": db.stat().st_size})
    except BaseException:
        # The copy is this step's own scratch; a failure must not strand the clean fixtures.
        shutil.rmtree(target, ignore_errors=True)
        shutil.rmtree(target.parent / (target.name + "-cache"), ignore_errors=True)
        shutil.rmtree(root / (SCALE_MEDIA + ".partial"), ignore_errors=True)
        raise
    print(f"{name} built: {added['BaseItems']} items cloned, " + ", ".join(
        f"{table} {rows}" for table, rows in sorted(added.items()) if table != "BaseItems"), flush=True)


# ── what must survive ───────────────────────────────────────────────────────
def counts(db):
    """Rows per table that hang off a clone (BaseItems and every CHILDREN table, UserData)."""
    with closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as conn:
        clone = f'SELECT "Id" FROM "BaseItems" WHERE "Path" LIKE \'{CLONE_MARK}\''
        result = {"BaseItems": conn.execute(f'SELECT COUNT(*) FROM "BaseItems" WHERE "Path" LIKE \'{CLONE_MARK}\'').fetchone()[0]}
        for table in (*CHILDREN, "UserData"):
            if table in tables_of(conn):
                column = CHILDREN.get(table, ("ItemId",))[0]
                result[table] = conn.execute(f'SELECT COUNT(*) FROM "{table}" WHERE "{column}" IN ({clone})').fetchone()[0]
    return result


def compare(before, after):
    """A failure per table that lost rows of a clone (rows only the later database holds are fine)."""
    failures = []
    for table, was in before.items():
        now = after.get(table, 0)
        if now < was:
            failures.append(f"{table}: {was} rows before, {now} after")
    return failures


# ── disk ────────────────────────────────────────────────────────────────────
def allocated(path):
    """Bytes the files under `path` occupy on disk (what du counts), links not followed."""
    total = 0
    for folder, _, files in os.walk(path):
        for name in files:
            try:
                total += os.lstat(os.path.join(folder, name)).st_blocks * 512
            except OSError:
                pass  # removed between listing and stat: it no longer occupies anything
    return total


def free_space_for(path):
    """Free bytes on the filesystem holding `path`, or its nearest existing parent."""
    path = Path(path)
    while not path.exists():
        path = path.parent
    return shutil.disk_usage(path).free


def format_bytes(count):
    for unit in ("B", "KiB", "MiB", "GiB"):
        if count < 1024 or unit == "GiB":
            return f"{count:.0f} {unit}" if unit == "B" else f"{count:.1f} {unit}"
        count /= 1024


def disk_shortfall(fixture, work, free=None):
    """None when `work` can hold a run of `fixture`, else why not."""
    db = Path(fixture) / "data/jellyfin.db"
    base = db.stat().st_size + sum(p.stat().st_size for p in db.parent.glob("jellyfin.db-wal"))
    rest = max(allocated(fixture) - base, 0)
    need, have = FREE_MULTIPLE * base + rest, (free or free_space_for)(work)
    if have >= need:
        return None
    return (f"needs {format_bytes(need)} free in {work} ({FREE_MULTIPLE}x the {format_bytes(base)} database "
            f"plus {format_bytes(rest)} for the rest of the fixture), has {format_bytes(have)}")


# ── measuring ───────────────────────────────────────────────────────────────
def locate_peak(container, root=Path("/sys/fs/cgroup"), proc=Path("/proc")):
    """The memory.peak file of the container's cgroup (cgroup v2)."""
    try:
        pid = subprocess.run(["docker", "inspect", "-f", "{{.State.Pid}}", container], capture_output=True,
                             text=True, check=True).stdout.strip()
        lines = (proc / pid / "cgroup").read_text().splitlines()
    except (subprocess.CalledProcessError, OSError) as error:
        raise CheckError(f"cannot find the cgroup of {container}: {error}") from error
    unified = [line.split("::", 1)[1] for line in lines if line.startswith("0::")]
    if not unified or pid == "0":
        raise CheckError(f"{container} has no cgroup v2 path (is it running?)")
    peak = root / unified[0].lstrip("/") / "memory.peak"
    if not peak.exists():
        raise CheckError(f"{peak} does not exist: memory.peak needs cgroup v2 and Linux 5.19")
    return peak


def anonymous_bytes(stat):
    """The `anon` line of a cgroup's memory.stat: memory no file backs."""
    for line in Path(stat).read_text().splitlines():
        name, _, value = line.partition(" ")
        if name == "anon":
            return int(value)
    raise ValueError(f"{stat} has no anon line")


def sample(container, directory, should_stop, interval=SAMPLE_S, locate=locate_peak, sleep=time.sleep):
    """Largest disk size of `directory`, of its WAL, the cgroup's memory peak and the largest anonymous
    memory seen, sampled until `should_stop()`, with one last sample after it. A value never seen is
    None: `memory_bytes` with `memory_error` when the cgroup was never readable, `wal_bytes` when no
    sample found a WAL."""
    result = {"disk_bytes": 0, "wal_bytes": None, "memory_bytes": None, "anon_bytes": None,
              "memory_error": None, "anon_error": None, "samples": 0}
    peak = None

    def take():
        nonlocal peak
        result["samples"] += 1
        result["disk_bytes"] = max(result["disk_bytes"], allocated(directory))
        try:
            wal = (Path(directory) / "data/jellyfin.db-wal").stat().st_blocks * 512
            result["wal_bytes"] = max(result["wal_bytes"] or 0, wal)
        except OSError:
            pass  # SQLite removes the WAL when the migrations' connection closes, before the pools open
        try:
            peak = peak or locate(container)
            result["memory_bytes"] = max(result["memory_bytes"] or 0, int(peak.read_text()))
        except (CheckError, OSError, ValueError) as error:
            result["memory_error"] = str(error)
            peak = None
            return
        try:
            result["anon_bytes"] = max(result["anon_bytes"] or 0, anonymous_bytes(peak.with_name("memory.stat")))
        except (OSError, ValueError) as error:
            result["anon_error"] = str(error)

    while not should_stop():
        take()
        sleep(interval)
    take()
    if result["memory_bytes"] is not None:
        result["memory_error"] = None
    if result["anon_bytes"] is not None:
        result["anon_error"] = None
    return result


def boot_seconds(log):
    """Seconds from "ferrofin-server starting" to "database migrations applied" in a JSON boot log."""
    stamps = {}
    for line in log.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        message = (event.get("fields") or {}).get("message")
        if message in ("ferrofin-server starting", "database migrations applied") and message not in stamps:
            stamps[message] = datetime.fromisoformat(event["timestamp"])
    if len(stamps) < 2:
        raise CheckError("the boot log does not hold both the server start and the applied migrations")
    return (stamps["database migrations applied"] - stamps["ferrofin-server starting"]).total_seconds()


def foreign_key_seconds(db):
    """Time of `PRAGMA foreign_key_check` on the adopted file: the cost the every-boot check adds."""
    with closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as conn:
        start = time.monotonic()
        conn.execute("PRAGMA foreign_key_check").fetchall()
        return time.monotonic() - start


def record(log, samples, db):
    """The numbers of one adoption, and why any of them is missing."""
    result = {"migration_seconds": None, "memory_bytes": samples.get("memory_bytes"),
              "anon_bytes": samples.get("anon_bytes"), "disk_bytes": samples.get("disk_bytes") or None,
              "wal_bytes": samples.get("wal_bytes"), "foreign_key_check_seconds": None, "missing": []}
    try:
        result["migration_seconds"] = boot_seconds(log)
    except (CheckError, KeyError, ValueError) as error:
        result["missing"].append(f"migration time: {error}")
    if result["memory_bytes"] is None:
        result["missing"].append("peak memory: " + str(samples.get("memory_error") or "never sampled"))
    elif result["anon_bytes"] is None:
        result["missing"].append("anonymous memory: " + str(samples.get("anon_error") or "never sampled"))
    if result["disk_bytes"] is None:
        result["missing"].append("disk high-water: never sampled")
    elif result["wal_bytes"] is None:
        result["missing"].append(f"WAL peak: none of {samples.get('samples', 0)} samples found a WAL")
    try:
        result["foreign_key_check_seconds"] = foreign_key_seconds(db)
    except sqlite3.Error as error:
        result["missing"].append(f"foreign_key_check: {error}")
    return result


def summary(result):
    seconds = lambda value: "not measured" if value is None else f"{value:.1f} s"
    size = lambda value: "not measured" if value is None else format_bytes(value)
    return (f"migration {seconds(result['migration_seconds'])}; peak memory {size(result['memory_bytes'])} "
            f"(cgroup, page cache included; anonymous {size(result['anon_bytes'])}); "
            f"disk high-water {size(result['disk_bytes'])} (WAL {size(result['wal_bytes'])}); "
            f"foreign_key_check {seconds(result['foreign_key_check_seconds'])}")


# ── command line ────────────────────────────────────────────────────────────
def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="mode", required=True)
    builder = sub.add_parser("build")
    builder.add_argument("--fixtures", required=True, type=Path)
    builder.add_argument("--items", type=int, default=ITEMS)
    disk = sub.add_parser("need-disk")
    disk.add_argument("fixture", type=Path)
    disk.add_argument("work", type=Path)
    snapshot = sub.add_parser("counts")
    snapshot.add_argument("db", type=Path)
    snapshot.add_argument("out", type=Path)
    checker = sub.add_parser("check")
    checker.add_argument("db", type=Path)
    checker.add_argument("baseline", type=Path)
    sampler = sub.add_parser("sample")
    sampler.add_argument("container")
    sampler.add_argument("directory", type=Path)
    sampler.add_argument("out", type=Path)
    recorder = sub.add_parser("record")
    recorder.add_argument("log", type=Path)
    recorder.add_argument("samples", type=Path)
    recorder.add_argument("db", type=Path)
    recorder.add_argument("out", type=Path)
    args = parser.parse_args(argv)
    os.umask(0o077)
    try:
        if args.mode == "build":
            build(args.fixtures.resolve(), args.items)
        elif args.mode == "need-disk":
            reason = disk_shortfall(args.fixture, args.work)
            if reason:
                print(reason)
                return 1
        elif args.mode == "counts":
            save(args.out, {"version": 1, "counts": counts(args.db)})
        elif args.mode == "check":
            baseline = json.loads(args.baseline.read_text())
            failures = compare(baseline["counts"], counts(args.db))
            if baseline["counts"].get("BaseItems", 0) == 0:
                failures.append("the baseline holds no cloned item")
            for line in failures:
                print(line)
            return int(bool(failures))
        elif args.mode == "sample":
            stopped = []
            for number in (signal.SIGTERM, signal.SIGINT):
                signal.signal(number, lambda *_: stopped.append(True))
            save(args.out, sample(args.container, args.directory, lambda: bool(stopped)))
        else:
            try:
                samples = json.loads(args.samples.read_text())
            except FileNotFoundError:
                # The sampler is stopped as soon as the boot fails, possibly before it has started.
                samples = {"memory_error": "the sampler left no samples"}
            result = record(args.log.read_text(), samples, args.db)
            save(args.out, result)
            print(summary(result))
            for line in result["missing"]:
                print("not measured: " + line)
            return int(bool(result["missing"]))
    except (CheckError, OSError, ValueError, KeyError, sqlite3.Error) as error:
        print(f"scale: {type(error).__name__}: {error}")
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
