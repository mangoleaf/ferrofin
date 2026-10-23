#!/usr/bin/env python3
"""Messy real-world rows, first slice: shapes long-lived Jellyfin databases carry that no API
can produce, injected by SQL into a STOPPED copy of a finished fixture, each with the end state
Jellyfin 12.2 gives it.

    messy.py build --fixtures DIR [--port N]    derive the messy fixtures, seed them, boot Jellyfin
                                                12.2 on clean copies carrying each shape to record
                                                what it does
    messy.py check DB FIXTURE_DIR               compare an adopted database with that record

Shapes (SHAPES), one function and one manifest entry each:
  deleted_user          UserData, Permissions, Preferences, Devices and DisplayPreferences rows of
                        a user that no longer exists
  dangling_owner_extra  an extra whose OwnerId names an item that does not exist
  duplicate_paths       two BaseItems rows with one Path

Each is seeded into the latest 10.11.x fixture and the latest 12.x fixture (DERIVED), one per
lineage (the 10.11 lineage runs the 0032 / FixIncorrectOwnerIdRelationships migrations on
adoption; a 12.x database has already run them). The derived fixtures are copies, so the eleven clean
paths and the smoke oracle recorded from a clean library stay the control.

The expectation per shape is the state Jellyfin 12.2 itself leaves (messy-oracle.json: it boots a
clean copy carrying that one shape and the harness reads the database afterwards). Where Ferrofin is
expected to differ, DIVERGENCES says so, with the state it must reach and why; a difference not
written down fails. Where 12.2 refuses a shape, the oracle records that, and Ferrofin's state is
then whatever DIVERGENCES names. Stdlib plus the harness's own modules.
"""

import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import time
import uuid

from metadata import CheckError
from synthetic_fixture import USERS, Api, seed_id

#: The shapes of this slice, in the order they are seeded.
SHAPES = ("deleted_user", "dangling_owner_extra", "duplicate_paths")
#: Derived fixture -> (source fixture, lineage). D1: the latest 10.11.x and the latest 12.x.
DERIVED = (("jellyfin-10.11.11-messy", "jellyfin-10.11.11", "10.11"),
           ("jellyfin-12.2-messy", "jellyfin-12.2-from-121", "12.x"))
#: What Jellyfin 12.2 runs for the oracle (pulled, and its image id recorded, when it is built).
ORACLE_IMAGE = "jellyfin/jellyfin:12.2"
#: How long the oracle waits for Jellyfin to answer, the deadline Api.ready gives it; and the grace
#: `docker stop` gives it to flush the database, the grace synthetic.server gives it.
ORACLE_READY_S = 300
STOP_GRACE_S = 60
#: The refusal reason keeps the last matching line of this many log lines, cut to this many characters.
LOG_TAIL = 400
REASON_CHARS = 200
#: The duplicate path's copy is dated before any row of any fixture, so the original is the newer
#: row, which is the one Jellyfin's rule keeps when neither row has children or extras.
OLDEST_DATE = "2000-01-01"
#: Seeded rows per table that have more than one to copy: two UserData items is enough to count.
USER_DATA_ROWS = 2
#: The tables holding rows of a deleted user (shape deleted_user).
USER_TABLES = ("UserData", "Permissions", "Preferences", "Devices", "DisplayPreferences")
VIDEO_TYPE = "MediaBrowser.Controller.Entities.Video"
PLACEHOLDER = "00000000000000000000000000000001"
#: The ghost user's rows are cloned from this fixture user.
SOURCE_USER = USERS[1]
#: Jellyfin's DisplayPreferences entity defaults (ShowSidebar, ShowBackdrop, ScrollDirection,
#: SkipForwardLength ms, SkipBackwardLength ms, ChromecastVersion, EnableNextVideoInfoOverlay), for a
#: fixture none of whose users has saved one.
DISPLAY_PREFERENCE_DEFAULTS = (0, 1, 0, 30000, 10000, 0, 1)


# ── seeding ─────────────────────────────────────────────────────────────────
def spelled(like, value):
    """`value` (a UUID) written the way the stored id `like` is: dashed or not, upper or lower.
    The same helper is nested in synthetic_fixture.seed_database; it is not shared because that
    file is hashed into the fixture recipe, and editing it would invalidate every fixture."""
    text = str(value) if "-" in like else value.hex
    return text.upper() if like == like.upper() else text


def hex_of(value):
    return None if value is None else uuid.UUID(value).hex


def clone_rows(conn, table, where, params, changes, limit=None):
    """Copy the rows of `table` matching `where` with `changes` applied, leaving an integer
    primary key (a rowid alias) to SQLite; returns how many it wrote."""
    cursor = conn.execute(f'SELECT * FROM "{table}" WHERE {where}' + (f" LIMIT {limit}" if limit else ""), params)
    names = [column[0] for column in cursor.description]
    rows = cursor.fetchall()
    info = list(conn.execute(f'PRAGMA table_info("{table}")'))
    keys = [column for column in info if column[5]]
    alias = keys[0][1] if len(keys) == 1 and keys[0][2].upper() == "INTEGER" else None
    for row in rows:
        values = dict(zip(names, row))
        values.update(changes)
        if alias:
            values.pop(alias, None)
        columns = ", ".join(f'"{name}"' for name in values)
        conn.execute(f'INSERT INTO "{table}" ({columns}) VALUES ({", ".join("?" * len(values))})', tuple(values.values()))
    return len(rows)


def stored_user(conn, manifest):
    wanted = uuid.UUID(manifest["users"][SOURCE_USER]).hex
    row = conn.execute('SELECT "Id" FROM "Users" WHERE lower(replace("Id", \'-\', \'\')) = ?', (wanted,)).fetchone()
    if row is None:
        raise CheckError("messy seed: the fixture has no " + SOURCE_USER)
    return row[0]


def seed_deleted_user(conn, manifest):
    """Rows of a user that no longer exists: UserData (two items), the Permissions and
    Preferences of the account they were copied from, one device and one display preference.
    Foreign keys must be off: the user row is not there."""
    source = stored_user(conn, manifest)
    ghost = seed_id("messy-deleted-user")
    like = conn.execute('SELECT "Id" FROM "Users" LIMIT 1').fetchone()[0]
    changes = {"UserId": spelled(like, ghost)}
    seeded = {
        # Ordered, so a rebuild copies the same rows.
        "UserData": clone_rows(conn, "UserData", '"UserId" = ? ORDER BY "ItemId", "CustomDataKey"', (source,),
                               changes, USER_DATA_ROWS),
        "Permissions": clone_rows(conn, "Permissions", '"UserId" = ?', (source,), changes),
        "Preferences": clone_rows(conn, "Preferences", '"UserId" = ?', (source,), changes),
        "Devices": clone_rows(conn, "Devices", '"UserId" = ? ORDER BY "Id"', (source,),
                              {**changes, "DeviceId": "synthetic-messy-deleted-device",
                               "AccessToken": seed_id("messy-deleted-token").hex}, 1),
        "DisplayPreferences": clone_rows(conn, "DisplayPreferences", '"UserId" = ? ORDER BY "Id"', (source,),
                                         changes, 1),
    }
    if not seeded["DisplayPreferences"]:
        seeded["DisplayPreferences"] = clone_rows(conn, "DisplayPreferences", '1 = 1 ORDER BY "Id"', (), changes, 1)
    if not seeded["DisplayPreferences"]:
        # The fixture's users never saved a display preference (web saves them on a settings
        # change, which the seed does not make): write one with the entity's own defaults.
        # ItemId is the empty Guid (a user-wide preference), in the stored spelling.
        conn.execute('INSERT INTO "DisplayPreferences" ("UserId", "Client", "ItemId", "ShowSidebar", "ShowBackdrop", '
                     '"ScrollDirection", "SkipForwardLength", "SkipBackwardLength", "ChromecastVersion", '
                     '"EnableNextVideoInfoOverlay") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)',
                     (changes["UserId"], "synthetic-messy", spelled(like, uuid.UUID(int=0)),
                      *DISPLAY_PREFERENCE_DEFAULTS))
        seeded["DisplayPreferences"] = 1
    missing = [table for table, count in seeded.items() if not count]
    if missing:
        raise CheckError("messy seed: nothing to copy for " + ", ".join(missing))
    return {"user": ghost.hex, "rows": seeded}


def seed_dangling_owner_extra(conn, manifest):
    """A copy of an existing extra that sits in a directory of its own and names an owner that
    does not exist, under a path no file has (nothing in the library scan can remove it first)."""
    row = conn.execute('SELECT "Id", "OwnerId" FROM "BaseItems" WHERE "ExtraType" IS NOT NULL AND "ExtraType" <> 0 '
                       'AND "Type" = ? ORDER BY "Id" LIMIT 1', (VIDEO_TYPE,)).fetchone()
    if row is None:
        raise CheckError("messy seed: the fixture has no extra to copy")
    extra, missing = seed_id("messy-extra"), seed_id("messy-missing-owner")
    clone_rows(conn, "BaseItems", '"Id" = ?', (row[0],), {
        # an extra with no owner reads its id's spelling from its own id
        "Id": spelled(row[0], extra), "OwnerId": spelled(row[1] or row[0], missing),
        "Name": "Synthetic dangling extra", "PresentationUniqueKey": extra.hex,
        "Path": "/media/movies/Synthetic dangling extras/extra.mkv"})
    return {"extra": extra.hex, "owner": missing.hex}


def seed_duplicate_paths(conn, manifest):
    """Two rows for one path: a copy of the private movie that is the older of the two, so
    Jellyfin's rule (keep the newer when neither has children or extras) keeps the original."""
    original = uuid.UUID(manifest["private_movie"])
    row = conn.execute('SELECT "Id", "Path", "DateCreated" FROM "BaseItems" WHERE lower(replace("Id", \'-\', \'\')) = ?',
                       (original.hex,)).fetchone()
    if row is None or not row[1]:
        raise CheckError("messy seed: the fixture has no movie to duplicate")
    duplicate = seed_id("messy-duplicate")
    # in the spelling the fixture's own dates use: the space form, or 0007's earlier ISO 'T' form
    older = OLDEST_DATE + (" 00:00:00.0000000" if " " in (row[2] or " ") else "T00:00:00.0000000Z")
    clone_rows(conn, "BaseItems", '"Id" = ?', (row[0],), {
        "Id": spelled(row[0], duplicate), "PresentationUniqueKey": duplicate.hex,
        "DateCreated": older})
    return {"path": row[1], "original": original.hex, "duplicate": duplicate.hex}


SEEDERS = {"deleted_user": seed_deleted_user, "dangling_owner_extra": seed_dangling_owner_extra,
           "duplicate_paths": seed_duplicate_paths}


def seed_messy(db, manifest, shapes=SHAPES):
    """Write `shapes` into the STOPPED database `db`; returns the manifest entry naming the rows."""
    entry = {"version": 1, "shapes": {}}
    with closing(sqlite3.connect(db)) as conn:
        # A deleted user and a missing owner need foreign keys off, outside a transaction.
        conn.execute("PRAGMA foreign_keys = OFF")
        with conn:
            for shape in shapes:
                entry["shapes"][shape] = SEEDERS[shape](conn, manifest)
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    return entry


# ── end state ───────────────────────────────────────────────────────────────
def user_rows(conn, table, user):
    return conn.execute(f'SELECT COUNT(*) FROM "{table}" WHERE lower(replace("UserId", \'-\', \'\')) = ?', (user,)).fetchone()[0]


def item(conn, wanted):
    return conn.execute('SELECT "Id", "OwnerId" FROM "BaseItems" WHERE lower(replace("Id", \'-\', \'\')) = ?',
                        (wanted,)).fetchone()


def owner_kind(conn, extra):
    """How an extra's OwnerId reads: none, self, placeholder, dangling (names no item) or other."""
    owned = hex_of(extra[1])
    if owned is None:
        return "none"
    if owned == hex_of(extra[0]):
        return "self"
    if owned == PLACEHOLDER:
        return "placeholder"
    return "other" if item(conn, owned) else "dangling"


def shape_state(conn, entry):
    """The end state of the seeded shapes in `conn` (a database any server has had since)."""
    state = {}
    shapes = entry["shapes"]
    if "deleted_user" in shapes:
        user = shapes["deleted_user"]["user"]
        state["deleted_user"] = {table: user_rows(conn, table, user) for table in USER_TABLES}
    if "dangling_owner_extra" in shapes:
        extra = item(conn, shapes["dangling_owner_extra"]["extra"])
        state["dangling_owner_extra"] = {"exists": extra is not None,
                                         "owner": owner_kind(conn, extra) if extra else None}
    if "duplicate_paths" in shapes:
        paths = shapes["duplicate_paths"]
        state["duplicate_paths"] = {"original": item(conn, paths["original"]) is not None,
                                    "duplicate": item(conn, paths["duplicate"]) is not None}
    return state


def read_state(db, entry):
    with closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as conn:
        return shape_state(conn, entry)


# ── what Ferrofin is expected to differ in ──────────────────────────────────
# (shape, lineage) -> (the state Ferrofin must reach, why it differs from Jellyfin 12.2). A
# difference from Jellyfin 12.2 that is not here fails the run. Read off the oracle's record
# (messy-oracle.json) and a live adoption; the dangling owner on the 12.x lineage is what Ferrofin
# must reach, not what it does today (gl-707).
NO_USER_ROWS = {table: 0 for table in USER_TABLES}
DIVERGENCES = {
    # Jellyfin 12.2 deletes only the Permissions and Preferences of a missing user (12.1's cleanup)
    # when it upgrades a 10.11 database, and nothing when it boots a 12.x one: UserData, Devices
    # and DisplayPreferences stay, rows ON DELETE CASCADE would have removed. Ferrofin's every-boot
    # foreign_key_check refuses such a database, so migration 0044 deletes them all (the owner's
    # decision, gl-438), with a per-table count in the boot log.
    ("deleted_user", "10.11"): (NO_USER_ROWS, "0044 deletes every row of a missing user; Jellyfin 12.2 keeps some"),
    ("deleted_user", "12.x"): (NO_USER_ROWS, "0044 deletes every row of a missing user; Jellyfin 12.2 keeps them all"),
    # Jellyfin 12.2 keeps an extra whose owner is gone on a 12.x database (the routines that
    # remove it ran when that database was upgraded), and boots. Ferrofin's foreign_key_check
    # refuses a dangling OwnerId, so the extra must be removed as on the 10.11 lineage, where 12.2
    # deletes it too. Ferrofin refuses the database instead today: gl-707.
    ("dangling_owner_extra", "12.x"): ({"exists": False, "owner": None},
                                       "Ferrofin removes a dangling-owner extra (as 12.2 does from 10.11); 12.2 keeps it on 12.x"),
    # A 12.x database has already run Jellyfin's FixIncorrectOwnerIdRelationships, so 12.2 keeps both
    # rows. Ferrofin runs its port once per adoption on any source and keeps the row that routine
    # keeps (children, then extras, then the newer one), deleting the other.
    ("duplicate_paths", "12.x"): ({"original": True, "duplicate": False},
                                  "Ferrofin collapses duplicate paths at adoption; 12.2 does not re-run its routine on 12.x"),
}


def expected(shape, lineage, oracle):
    """(state, source) Ferrofin must reach for `shape`: a written divergence, else Jellyfin 12.2's."""
    if (shape, lineage) in DIVERGENCES:
        state, why = DIVERGENCES[(shape, lineage)]
        return state, "written divergence: " + why
    record = oracle["shapes"][shape]
    if record["outcome"] != "booted":
        return None, f"Jellyfin 12.2 {record['outcome']} this shape ({record.get('reason', '')}) and no divergence says what Ferrofin should do"
    return record["state"][shape], "Jellyfin 12.2"


def compare(state, entry, oracle, lineage):
    """Failures of an adopted database's shape `state`, one line per shape that is not what it must be."""
    failures = []
    for shape in entry["shapes"]:
        want, source = expected(shape, lineage, oracle)
        if want is None:
            failures.append(f"{shape}: {source}")
        elif state[shape] != want:
            failures.append(f"{shape}: {state[shape]}, expected {want} ({source})")
    return failures


def shape_results(db, fixture):
    """One dict per shape of an adopted database: its end state, what it must be, where that comes from
    and the verdict. `compare` holds the same judgement as the failures `check` prints."""
    entry = json.loads((fixture / "messy.json").read_text())
    oracle = json.loads((fixture / "messy-oracle.json").read_text())
    if set(entry["shapes"]) != set(SHAPES):
        # A fixture built before a shape was added would leave that shape silently unchecked.
        # (The manifest is saved with sorted keys, so only the set of shapes is comparable.)
        raise CheckError(f"the fixture was built with shapes {', '.join(sorted(entry['shapes']))}, not "
                         f"{', '.join(SHAPES)}; build the messy fixtures again")
    state = read_state(db, entry)
    results = []
    for shape in entry["shapes"]:
        want, source = expected(shape, entry["lineage"], oracle)
        results.append({"shape": shape, "got": state[shape], "want": want, "source": source,
                        "verdict": "PASS" if want is not None and state[shape] == want else "FAIL"})
    return results, compare(state, entry, oracle, entry["lineage"])


def check(db, fixture):
    return shape_results(db, fixture)[1]


# ── the oracle: Jellyfin 12.2 on a clean copy carrying one shape ────────────
def run_oracle_server(config, media, port, name="messy-oracle"):
    """Boot Jellyfin 12.2 on `config`, let it settle, stop it; returns {"outcome", "reason"}:
    booted, or refused when the container exits before it answers."""
    cache = config.parent / (config.name + "-cache")
    cache.mkdir(exist_ok=True)
    subprocess.run(["docker", "rm", "-f", name], capture_output=True)
    subprocess.run(["docker", "run", "-d", "--name", name, "--user", f"{os.getuid()}:{os.getgid()}",
                    "-p", f"127.0.0.1:{port}:8096", "-v", f"{config}:/config", "-v", f"{cache}:/cache",
                    "-v", f"{media}:/media:ro", "-e", "HTTP_PROXY=http://127.0.0.1:9",
                    "-e", "HTTPS_PROXY=http://127.0.0.1:9", "-e", "NO_PROXY=127.0.0.1,localhost",
                    ORACLE_IMAGE], check=True, stdout=subprocess.DEVNULL)
    api = Api(f"http://127.0.0.1:{port}")
    try:
        end = time.monotonic() + ORACLE_READY_S
        while True:
            running = subprocess.run(["docker", "inspect", "-f", "{{.State.Running}}", name],
                                     capture_output=True, text=True).stdout.strip()
            if running != "true":
                return {"outcome": "refused", "reason": refusal_reason(name)}
            try:
                api.get("/System/Ping")
                api.get("/System/Info/Public")
                break
            except (OSError, CheckError):
                # Still running and not answering is not a refusal: a slow host must not be
                # recorded as Jellyfin's behaviour.
                if time.monotonic() >= end:
                    raise CheckError(f"Jellyfin 12.2 did not answer within {ORACLE_READY_S} s and did not exit")
                time.sleep(1)
        api.login(USERS[0]).idle()
        return {"outcome": "booted", "reason": ""}
    finally:
        subprocess.run(["docker", "stop", "-t", str(STOP_GRACE_S), name], capture_output=True)
        with open(config.parent / (config.name + ".server.log"), "w") as output:
            subprocess.run(["docker", "logs", name], stdout=output, stderr=subprocess.STDOUT)
        subprocess.run(["docker", "rm", name], capture_output=True)


def refusal_reason(name):
    """Why the container exited: the last foreign-key line of its log, else the last error or
    exception line, shortened. The last one, since the first is often a benign line before it."""
    logs = subprocess.run(["docker", "logs", "--tail", str(LOG_TAIL), name], capture_output=True, text=True)
    lines = (logs.stdout + logs.stderr).splitlines()
    for marker in ("FOREIGN KEY", "[ERR]", "Exception"):
        found = [line for line in lines if marker in line]
        if found:
            return found[-1][:REASON_CHARS]
    return "no error line in the log"


def image_id():
    """The id of the oracle image the local tag names now."""
    return subprocess.run(["docker", "image", "inspect", "-f", "{{.Id}}", ORACLE_IMAGE],
                          capture_output=True, text=True, check=True).stdout.strip()


def record_oracle(root, source, entry_shapes, manifest, port):
    """For each shape, boot Jellyfin 12.2 on a clean copy of `source` carrying only that shape and
    record the outcome and the end state; the combined fixture is booted last."""
    # A stale local tag would record a different release without complaint: pull it, and keep its id.
    subprocess.run(["docker", "pull", "-q", ORACLE_IMAGE], check=True, stdout=subprocess.DEVNULL)
    work = root / "work"
    work.mkdir(exist_ok=True)
    record = {"image": ORACLE_IMAGE, "image_id": image_id(), "shapes": {}}
    for shape in (*entry_shapes, None):
        label = shape or "all"
        copy = work / f"messy-oracle-{source.name}-{label}"
        if copy.exists():
            raise CheckError(f"{copy} exists; remove it (it is scratch) and build again")
        try:
            shutil.copytree(source, copy)
            entry = seed_messy(copy / "data/jellyfin.db", manifest, (shape,) if shape else entry_shapes)
            outcome = run_oracle_server(copy, root / "media", port)
            outcome["state"] = read_state(copy / "data/jellyfin.db", entry)
        finally:
            shutil.rmtree(copy, ignore_errors=True)
            shutil.rmtree(copy.parent / (copy.name + "-cache"), ignore_errors=True)
        if shape:
            record["shapes"][shape] = outcome
        else:
            record["combined"] = outcome
    differing = [shape for shape in entry_shapes
                 if (record["shapes"][shape]["outcome"], record["shapes"][shape]["state"][shape])
                 != (record["combined"]["outcome"], record["combined"]["state"][shape])]
    if differing:
        # Ferrofin adopts the fixture carrying every shape, so the expectation must hold there too.
        raise CheckError("Jellyfin 12.2 handled " + ", ".join(differing) + " differently with all the shapes "
                         "present than with each alone; the per-shape record would be the wrong expectation")
    return record


def save(path, data):
    with open(path, "w") as output:
        os.fchmod(output.fileno(), 0o600)
        json.dump(data, output, indent=2, sort_keys=True)


def build(root, port):
    for name, source_name, lineage in DERIVED:
        target = root / name
        if (target / "messy-oracle.json").exists():
            built = set(json.loads((target / "messy.json").read_text())["shapes"])
            if built != set(SHAPES):
                raise CheckError(f"{name} was built with shapes {', '.join(sorted(built))}; "
                                 "build into a new fixtures directory")
            print(name + " already built", flush=True)
            continue
        source = root / source_name
        if not (source / "synthetic-ready.json").exists():
            raise CheckError(f"{source_name} is not built; run build-fixtures.sh first")
        manifest = json.loads((source / "synthetic.json").read_text())
        if target.exists():
            raise CheckError(f"{target} is incomplete; remove it (it is a derived copy) and build again")
        try:
            shutil.copytree(source, target)
            entry = seed_messy(target / "data/jellyfin.db", manifest)
            entry["lineage"] = lineage
            entry["source"] = source_name
            save(target / "messy.json", entry)
            oracle = record_oracle(root, source, SHAPES, manifest, port)
            save(target / "messy-oracle.json", oracle)
        except BaseException:
            # The copy is this step's own scratch; a failure must not strand the clean fixtures.
            shutil.rmtree(target, ignore_errors=True)
            shutil.rmtree(target.parent / (target.name + "-cache"), ignore_errors=True)
            raise
        print(f"{name} built: " + ", ".join(f"{shape} {oracle['shapes'][shape]['outcome']}" for shape in SHAPES), flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="mode", required=True)
    builder = sub.add_parser("build")
    builder.add_argument("--fixtures", required=True, type=Path)
    builder.add_argument("--port", type=int, default=18130)
    checker = sub.add_parser("check")
    checker.add_argument("db", type=Path)
    checker.add_argument("fixture", type=Path)
    shaper = sub.add_parser("shapes", help="print each shape's end state, expectation and verdict as JSON")
    shaper.add_argument("db", type=Path)
    shaper.add_argument("fixture", type=Path)
    args = parser.parse_args(argv)
    os.umask(0o077)
    try:
        if args.mode == "build":
            build(args.fixtures.resolve(), args.port)
            return 0
        if args.mode == "shapes":
            print(json.dumps(shape_results(args.db, args.fixture)[0], sort_keys=True))
            return 0
        failures = check(args.db, args.fixture)
        for line in failures:
            print(line)
        return int(bool(failures))
    except (CheckError, OSError, ValueError, KeyError, sqlite3.Error, subprocess.CalledProcessError) as error:
        print(f"messy rows: {type(error).__name__}: {error}")
        return 2


if __name__ == "__main__":
    sys.exit(main())
