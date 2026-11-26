#!/usr/bin/env python3
"""The native route: a database Ferrofin created itself, never adopted from Jellyfin (gl-1552).

    native.py seed --base-url URL           seed Ferrofin's first start on an empty directory as each fixture's
                                            Jellyfin is seeded (gates/adoption/synthetic_fixture.py seed: the
                                            wizard, its accounts, API key, web session, libraries and scan, a
                                            custom cast receiver among its settings), then set the TRAPS
    native.py decoy --data-dir DIR          write DIR/data/library.db.old, a legacy library database that names
                                            the first movie a folder and holds one user-data row of no item
    native.py drift --fresh DB --export DB  the history of DB, a fresh install of the Jellyfin release the export
                                            targets, against the export's: a line per id one records and the other
                                            does not, and per id recorded with another ProductVersion

A native database has no Jellyfin history, so the export gives it the one a fresh install records: every schema
migration, and every code migration as applied, so Jellyfin's first start runs none (owner decision on gl-1552).
The traps and the decoy are what Jellyfin's code migrations would change if the history left them to run: the
throttling DisableTranscodingThrottling turns off, the cast receivers AddDefaultCastReceivers replaces, the folder
flag ReseedFolderFlag copies from library.db.old and the user data MigrateLibraryUserData imports from it. A
database Ferrofin created has no library.db.old; one that lost its history, as the native rule must also take,
can. The port is a setting of the network page, which Jellyfin must serve on after the return (run.sh publishes
the exported one); MigrateNetworkConfiguration cannot read a 12.4-shaped network.xml and leaves it as it is.

The drift check is the version-drift guard (B11): the export writes, for a native database, exactly the ids its
classification of the target release does not retire (JELLYFIN_MIGRATIONS in
crates/ferrofin-db/src/export/classify.rs), so an id a fresh install of the release records and the export does
not is one the exporter does not classify, and a release that adds a migration fails here, by name.

Exit status 0 when done or nothing differs; 1 with one line per reason on stdout; 2 when a database cannot be
read.
"""

import argparse
from contextlib import closing
from pathlib import Path
import sqlite3
import sys

ADOPTION = Path(__file__).resolve().parent.parent / "adoption"
sys.path.append(str(ADOPTION))
from metadata import CheckError  # noqa: E402

#: What the seeding sets besides synthetic_fixture.seed's own settings, each a (document, setting, value) away from
#: its default: /System/Configuration/<document>. The port is any but the default 8096; Ferrofin keeps serving
#: where it started until it restarts, and Jellyfin serves on it after the return.
TRAPS = (("network", "InternalHttpPort", 8097), ("encoding", "EnableThrottling", True))
#: A file of the legacy library database's (Jellyfin 10.10 and earlier) that two code migrations read.
DECOY = "data/library.db.old"
#: The tables of it those migrations read, with the columns they read (MigrateLibraryUserData, ReseedFolderFlag).
DECOY_SCHEMA = (
    "CREATE TABLE TypedBaseItems (guid GUID PRIMARY KEY, type TEXT NOT NULL, IsFolder BIT, UserDataKey TEXT)",
    "CREATE TABLE UserDatas (key NVARCHAR NOT NULL, userId INT NOT NULL, rating FLOAT NULL, played BIT NOT NULL, "
    "playCount INT NOT NULL, isFavorite BIT NOT NULL, playbackPositionTicks BIGINT NOT NULL, "
    "lastPlayedDate DATETIME NULL, AudioStreamIndex INT NULL, SubtitleStreamIndex INT NULL)",
)
MOVIE = "MediaBrowser.Controller.Entities.Movies.Movie"
#: How many ids a reason names before it only counts the rest.
MAX_SHOWN = 5


def seed(base):
    """Seed the fresh server at base, then set the TRAPS as its administrator."""
    import synthetic_fixture   # Pillow, as for the fixtures' own build: only this mode needs it
    synthetic_fixture.seed(base)
    admin = synthetic_fixture.Api(base).login(synthetic_fixture.USERS[0])
    set_traps(admin)


def set_traps(api):
    """Each of TRAPS through api (get, post), read back: a setting the server did not keep is a CheckError."""
    for document, setting, value in TRAPS:
        path = f"/System/Configuration/{document}"
        settings = api.get(path)
        settings[setting] = value
        api.post(path, settings)
        if api.get(path).get(setting) != value:
            raise CheckError(f"{path}: {setting} was not kept as {value}")


def decoy(data_dir):
    """Write DECOY into data_dir from its database (read only): the first movie by name as a folder, and a user-data
    row of the first administrator's on a key no item has."""
    data_dir = Path(data_dir)
    with closing(sqlite3.connect((data_dir / "data" / "jellyfin.db").resolve().as_uri() + "?mode=ro", uri=True)) as db:
        movie = db.execute('SELECT "Id" FROM "BaseItems" WHERE "Type" = ? ORDER BY "Name" LIMIT 1', (MOVIE,)).fetchone()
        user = db.execute('SELECT u."InternalId" FROM "Users" u WHERE EXISTS (SELECT 1 FROM "Permissions" p '
                          'WHERE p."UserId" = u."Id" AND p."Kind" = 0 AND p."Value" = 1) ORDER BY u."InternalId" '
                          'LIMIT 1').fetchone()
    if not movie or not user:
        raise CheckError("the native source has no movie or no administrator to name in the decoy")
    path = data_dir / DECOY
    if path.exists():
        raise CheckError(f"{path} exists already")
    with closing(sqlite3.connect(path)) as db, db:
        for statement in DECOY_SCHEMA:
            db.execute(statement)
        db.execute("INSERT INTO TypedBaseItems VALUES (?, ?, 1, 'decoy-folder')", (movie[0], MOVIE))
        db.execute("INSERT INTO UserDatas VALUES ('decoy-detached', ?, NULL, 1, 1, 0, 0, NULL, NULL, NULL)", (user[0],))


def history(path):
    """{MigrationId: ProductVersion} of the database at path."""
    with closing(sqlite3.connect(Path(path).resolve().as_uri() + "?mode=ro", uri=True)) as db:
        return dict(db.execute('SELECT "MigrationId", "ProductVersion" FROM "__EFMigrationsHistory"'))


def drift(fresh, export):
    """The reasons the export's history ({id: version}) is not the fresh install's."""
    if not fresh:
        return ["the fresh install recorded no history: its start did not complete"]
    reasons = []
    if missing := sorted(set(fresh) - set(export)):
        reasons.append(f"{len(missing)} id(s) a fresh install records are not in the export's history, so the "
                       f"exporter does not classify them (or retires them): {shown(missing)}; classify each in "
                       f"JELLYFIN_MIGRATIONS (crates/ferrofin-db/src/export/classify.rs)")
    if extra := sorted(set(export) - set(fresh)):
        reasons.append(f"{len(extra)} id(s) the export records that a fresh install does not: {shown(extra)}")
    if other := sorted(key for key in set(fresh) & set(export) if fresh[key] != export[key]):
        reasons.append(f"{len(other)} id(s) recorded with another ProductVersion than a fresh install's: " + shown(
            [f"{key} ({export[key]}, not {fresh[key]})" for key in other]))
    return reasons


def shown(names):
    more = f"; and {len(names) - MAX_SHOWN} more" if len(names) > MAX_SHOWN else ""
    return ", ".join(names[:MAX_SHOWN]) + more


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="mode", required=True)
    sub.add_parser("seed").add_argument("--base-url", required=True)
    sub.add_parser("decoy").add_argument("--data-dir", required=True)
    compare = sub.add_parser("drift")
    compare.add_argument("--fresh", required=True)
    compare.add_argument("--export", required=True)
    args = parser.parse_args(argv)
    if args.mode == "drift":
        try:
            reasons = drift(history(args.fresh), history(args.export))
        except sqlite3.Error as error:
            print(f"cannot read a history: {error}")
            return 2
        for reason in reasons:
            print(reason)
        return int(bool(reasons))
    try:
        if args.mode == "seed":
            seed(args.base_url)
        else:
            decoy(args.data_dir)
    except (CheckError, OSError, sqlite3.Error, KeyError, ValueError) as error:
        print(f"{args.mode}: {type(error).__name__}: {error}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
