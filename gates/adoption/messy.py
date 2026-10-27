#!/usr/bin/env python3
"""Messy real-world rows: shapes long-lived Jellyfin databases carry that no API
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
  guid_casing           owner and primary-version ids in lower-case and 32-character spellings
  case_duplicates       people, a person item and an artist whose names differ only in case
  orphan_ancestor       AncestorIds rows naming a parent or an item that does not exist
  virtual_season        a season with no folder, numbered past the highest real one
  gone_file_userdata    an item whose file is gone, with play state kept
  non_ascii_names       users, a person and a library path no ASCII-only code survives

Items that need a file are given one in a hard-linked copy of the shared media tree
(DIR/media-messy), which the oracle and the messy rows mount in place of it.

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

#: The shapes, in the order they are seeded.
SHAPES = ("deleted_user", "dangling_owner_extra", "duplicate_paths", "guid_casing", "case_duplicates",
          "orphan_ancestor", "virtual_season", "gone_file_userdata", "non_ascii_names")
#: Derived fixture -> (source fixture, lineage): the latest 10.11.x and the latest 12.x.
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
#: Users whose play state the item of gone_file_userdata keeps: two is enough to count.
GONE_FILE_USERS = 2
#: The tables holding rows of a deleted user (shape deleted_user).
USER_TABLES = ("UserData", "Permissions", "Preferences", "Devices", "DisplayPreferences")
VIDEO_TYPE = "MediaBrowser.Controller.Entities.Video"
MOVIE_TYPE = "MediaBrowser.Controller.Entities.Movies.Movie"
PLACEHOLDER = "00000000000000000000000000000001"
#: The ghost user's rows are cloned from this fixture user.
SOURCE_USER = USERS[1]
#: Jellyfin's DisplayPreferences entity defaults (ShowSidebar, ShowBackdrop, ScrollDirection,
#: SkipForwardLength ms, SkipBackwardLength ms, ChromecastVersion, EnableNextVideoInfoOverlay), for a
#: fixture none of whose users has saved one.
DISPLAY_PREFERENCE_DEFAULTS = (0, 1, 0, 30000, 10000, 0, 1)


#: The media tree the messy rows mount as /media: a hard-linked copy of the fixtures' own, plus a link
#: for each file a seeded item needs. The clean rows scan the shared tree, so it stays as it is.
MESSY_MEDIA = "media-messy"


def link_media(root, entry):
    """Make `root`/MESSY_MEDIA (a hard-linked copy of `root`/media on first use) hold every file the
    seeded shapes in `entry` list as [path, template path] (container paths under /media): the new
    path is a hard link to the template's file. A template that is not there is an error."""
    tree = root / MESSY_MEDIA
    if not tree.exists():
        # Built beside and renamed into place, so an interrupted copy is never taken for a whole tree
        # (an item whose file is missing would be removed by Jellyfin's upgrade, which is what the tree is for).
        partial = root / (MESSY_MEDIA + ".partial")
        shutil.rmtree(partial, ignore_errors=True)
        shutil.copytree(root / "media", partial, copy_function=os.link)
        partial.rename(tree)
    for shape in entry["shapes"].values():
        for path, template in shape.get("files", ()):
            if not (path.startswith("/media/") and template.startswith("/media/")):
                raise CheckError(f"messy media: {path} and {template} must be container paths under /media/")
            source, target = tree / template.removeprefix("/media/"), tree / path.removeprefix("/media/")
            if not source.is_file():
                raise CheckError(f"messy media: the template file {template} is not in {tree}")
            target.parent.mkdir(parents=True, exist_ok=True)
            if not target.exists():
                os.link(source, target)


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
    does not exist. Its file exists (a link to the extra's, in the messy media tree): Jellyfin 12.2
    removes an item whose file is missing when it upgrades a 10.11 database, which would hide what
    it does with the owner."""
    row = conn.execute('SELECT "Id", "OwnerId", "Path" FROM "BaseItems" WHERE "ExtraType" IS NOT NULL AND "ExtraType" <> 0 '
                       f'AND "Type" = ? AND {original("BaseItems")} ORDER BY "Id" LIMIT 1', (VIDEO_TYPE,)).fetchone()
    if row is None:
        raise CheckError("messy seed: the fixture has no extra to copy")
    extra, missing = seed_id("messy-extra"), seed_id("messy-missing-owner")
    path = "/media/movies/Synthetic dangling extras/extra.mkv"
    clone_rows(conn, "BaseItems", '"Id" = ?', (row[0],), {
        # an extra with no owner reads its id's spelling from its own id
        "Id": spelled(row[0], extra), "OwnerId": spelled(row[1] or row[0], missing),
        "Name": "Synthetic dangling extra", "PresentationUniqueKey": extra.hex, "Path": path})
    return {"extra": extra.hex, "owner": missing.hex, "files": [[path, row[2]]]}


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


# ── the shapes of identifier spelling, duplicates and names ─────────────────
#: Three users whose names a naive case or Unicode mapping gets wrong, each for its own reason: the
#: dotted capital I of Turkish, the sharp s (which .NET's invariant upper-casing leaves alone and
#: ICU turns into SS) and an e with a combining diaeresis that no normalisation has composed.
UNICODE_USERS = {"dotted_capital_i": "\u0130stanbul", "sharp_s": "Stra\u00dfe", "decomposed": "Zoe\u0308"}
#: A person pair that differs only by an accented capital, and a name written decomposed.
UNICODE_PEOPLE = {"capital": "\u00c9mile Zola", "small": "\u00e9mile zola", "decomposed": "Zoe\u0308 Saldan\u0303a"}
#: A library path no ASCII-only code survives: an accent, a decomposed accent (U and a combining diaeresis,
#: which no normalisation has composed) and a symbol.
UNICODE_PATH = "/media/movies/U\u0308n\u00efcode \u2603 Movie (2020)/U\u0308n\u00efcode \u2603 Movie.mkv"
PERSON_TYPE = "MediaBrowser.Controller.Entities.Person"
ARTIST_TYPE = "MediaBrowser.Controller.Entities.Audio.MusicArtist"
SEASON_TYPE = "MediaBrowser.Controller.Entities.TV.Season"
EPISODE_TYPE = "MediaBrowser.Controller.Entities.TV.Episode"


def person_path(template, name):
    """The metadata path of the person `name`, laid out like `template`'s (`…/People/<initial>/<name>`):
    a case-variant name has a path of its own, so two persons never share one."""
    return f"{os.path.dirname(os.path.dirname(template))}/{name[0].upper()}/{name}"


def sid(like, name):
    """A fixed id for the seeded row `name`, spelled the way the stored id `like` is."""
    return spelled(like, seed_id(name))


def clone_one(conn, table, key, changes, what):
    """The one row of `table` with primary key `key` copied with `changes` (a CheckError when absent)."""
    if not clone_rows(conn, table, key[0], key[1], changes, 1):
        raise CheckError(f"messy seed: the fixture has no {what} to copy")


def first(conn, sql, params, what):
    row = conn.execute(sql, params).fetchone()
    if row is None:
        raise CheckError(f"messy seed: the fixture has no {what}")
    return row


def seed_guid_casing(conn, manifest):
    """Extras whose OwnerId and videos whose PrimaryVersionId read as a lower-case dashed id and as
    32 undashed characters, each naming an item that exists. Jellyfin writes upper-case dashed ids
    from 12.0 on; older databases hold other spellings, and 12.x converts them on upgrade only."""
    known = original("BaseItems", 'b."Id"')
    owner_exists = "EXISTS (SELECT 1 FROM \"BaseItems\" o WHERE lower(replace(o.\"Id\", '-', '')) = lower(replace(b.\"OwnerId\", '-', '')))"
    extra = first(conn, 'SELECT b."Id", b."OwnerId", b."Path" FROM "BaseItems" b WHERE b."ExtraType" IS NOT NULL AND b."ExtraType" <> 0 '
                  f'AND b."Type" = ? AND {known} AND {owner_exists} ORDER BY b."Id" LIMIT 1', (VIDEO_TYPE,), "extra with an owner to copy")
    owner = uuid.UUID(extra[1])
    primary_row = first(conn, 'SELECT "Id", "Path" FROM "BaseItems" WHERE lower(replace("Id", \'-\', \'\')) = ?',
                        (uuid.UUID(manifest["private_movie"]).hex,), "movie to name as a primary version")
    primary = uuid.UUID(primary_row[0])
    items, files = {}, []
    for label, source, template, column, value in (
            ("owner_lower36", extra[0], extra[2], "OwnerId", str(owner).lower()),
            ("owner_n32", extra[0], extra[2], "OwnerId", owner.hex),
            ("primary_lower36", primary_row[0], primary_row[1], "PrimaryVersionId", str(primary).lower()),
            ("primary_n32", primary_row[0], primary_row[1], "PrimaryVersionId", primary.hex)):
        ident = seed_id("messy-guid-" + label)
        path = f"/media/movies/Synthetic guid casing/{label}/{label}.mkv"
        clone_one(conn, "BaseItems", ('"Id" = ?', (source,)), {
            "Id": spelled(source, ident), column: value, "Name": "Synthetic guid casing " + label,
            "PresentationUniqueKey": ident.hex, "Path": path}, "item")
        items[label] = ident.hex
        files.append([path, template])
    return {"items": items, "owner": owner.hex, "primary": primary.hex, "files": files}


def seed_case_duplicates(conn, manifest):
    """Pairs of rows whose names differ only by case, as Jellyfin 12.0's MergeDuplicatePeople and
    MergeDuplicateMusicArtists routines look for, each pair independent of the others: a Person item
    in ASCII, a Peoples row in ASCII (linked to a movie), an accented pair of both (each linked), and
    a music artist. The pairs are read by lower-cased name, not by id: a server may re-key a person."""
    person = first(conn, 'SELECT "Id", "Name", "Path" FROM "BaseItems" WHERE "Type" = ? AND ' + original("BaseItems") + ' ORDER BY "Id" LIMIT 1',
                   (PERSON_TYPE,), "person")
    # By name, not by id: Jellyfin gives a Peoples row a random id, so the first by id differs per build.
    peoples = first(conn, 'SELECT "Id", "Name", "PersonType" FROM "Peoples" WHERE ' + original("Peoples") +
                    ' ORDER BY "Name", "PersonType", "Id" LIMIT 1', (), "people row")
    link = first(conn, 'SELECT "ItemId", "Role" FROM "PeopleBaseItemMap" WHERE "PeopleId" = ? ORDER BY "ItemId" LIMIT 1',
                 (peoples[0],), "movie linked to a person")
    artist = first(conn, 'SELECT "Id", "Name", "Path" FROM "BaseItems" WHERE "Type" = ? AND ' + original("BaseItems") + ' ORDER BY "Id" LIMIT 1',
                   (ARTIST_TYPE,), "music artist")

    def add_item(label, name):
        ident = seed_id("messy-case-item-" + label)
        clone_one(conn, "BaseItems", ('"Id" = ?', (person[0],)), {
            "Id": spelled(person[0], ident), "Name": name, "CleanName": name.lower(), "SortName": name,
            "PresentationUniqueKey": ident.hex, "Path": person_path(person[2], name)}, "person")

    def add_people(label, name):
        row = seed_id("messy-case-people-" + label)
        clone_one(conn, "Peoples", ('"Id" = ?', (peoples[0],)), {"Id": spelled(peoples[0], row), "Name": name}, "people row")
        clone_one(conn, "PeopleBaseItemMap", ('"PeopleId" = ? AND "ItemId" = ? AND "Role" = ?', (peoples[0], link[0], link[1])),
                  {"PeopleId": spelled(peoples[0], row)}, "people link")

    add_item("ascii", person[1].upper())
    add_people("ascii", peoples[1].upper())
    for label in ("capital", "small"):
        add_item("unicode-" + label, UNICODE_PEOPLE[label])
        add_people("unicode-" + label, UNICODE_PEOPLE[label])
    twin = seed_id("messy-case-artist")
    clone_one(conn, "BaseItems", ('"Id" = ?', (artist[0],)), {
        "Id": spelled(artist[0], twin), "Name": artist[1].upper(), "CleanName": artist[1].lower(), "SortName": artist[1].upper(),
        "PresentationUniqueKey": twin.hex, "Path": f"{os.path.dirname(artist[2])}/{artist[1].upper()}"}, "music artist")
    return {"movie": uuid.UUID(link[0]).hex,
            "keys": {"person_item_ascii": person[1].lower(), "people_ascii": peoples[1].lower(), "people_type": peoples[2],
                     "unicode": UNICODE_PEOPLE["small"], "artist": artist[1].lower()}}


def seed_orphan_ancestor(conn, manifest):
    """Two AncestorIds rows with an end that names no item: one whose parent is missing and one whose
    item is missing. The key is not enforced while foreign keys are off."""
    # A movie no other shape names or copies as an original: Jellyfin rewrites the ancestors of an item
    # whose versions change (guid_casing's versions name the private movie), which would drop the row.
    movie = first(conn, 'SELECT "Id" FROM "BaseItems" WHERE "Type" = ? AND lower(replace("Id", \'-\', \'\')) <> ? AND '
                  + original("BaseItems") + ' ORDER BY "Id" LIMIT 1', (MOVIE_TYPE, uuid.UUID(manifest["private_movie"]).hex), "movie other shapes leave alone")
    missing_parent, missing_item = seed_id("messy-orphan-parent"), seed_id("messy-orphan-item")
    conn.execute('INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?, ?)', (movie[0], spelled(movie[0], missing_parent)))
    conn.execute('INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?, ?)', (spelled(movie[0], missing_item), movie[0]))
    return {"movie": uuid.UUID(movie[0]).hex, "missing_parent": missing_parent.hex, "missing_item": missing_item.hex}


def seed_virtual_season(conn, manifest):
    """A season no folder backs (Path NULL) with one episode in it: what a series gets when its files
    skip a season. Its number is one past the series' highest, so it collides with no real season."""
    season = first(conn, 'SELECT "Id", "SeriesId" FROM "BaseItems" WHERE "Type" = ? AND ' + original("BaseItems") +
                   ' ORDER BY "IndexNumber" DESC, "Id" LIMIT 1', (SEASON_TYPE,), "season")
    number = conn.execute('SELECT MAX("IndexNumber") + 1 FROM "BaseItems" WHERE "Type" = ? AND "SeriesId" = ?',
                          (SEASON_TYPE, season[1])).fetchone()[0]
    episode = first(conn, 'SELECT "Id", "Path" FROM "BaseItems" WHERE "Type" = ? AND "SeasonId" = ? AND ' + original("BaseItems") +
                    ' ORDER BY "Id" LIMIT 1', (EPISODE_TYPE, season[0]), "episode of that season")
    new_season, new_episode = seed_id("messy-virtual-season"), seed_id("messy-virtual-episode")
    # In the series folder, not a folder of its own: a season no folder backs is what files laid out
    # as `Show S09E01.mkv` give, and a Season folder on disk would let a scan make a real season.
    episode_path = f"{os.path.dirname(os.path.dirname(episode[1]))}/Synthetic virtual season episode S{number:02d}E01.mkv"
    clone_one(conn, "BaseItems", ('"Id" = ?', (season[0],)), {
        "Id": spelled(season[0], new_season), "Name": f"Season {number}", "IndexNumber": number, "Path": None,
        "PresentationUniqueKey": new_season.hex}, "season")
    clone_one(conn, "BaseItems", ('"Id" = ?', (episode[0],)), {
        "Id": spelled(episode[0], new_episode), "Name": "Synthetic virtual season episode", "IndexNumber": 1,
        "ParentIndexNumber": number, "ParentId": spelled(season[0], new_season), "SeasonId": spelled(season[0], new_season),
        "PresentationUniqueKey": new_episode.hex,
        "Path": episode_path}, "episode")
    return {"series": uuid.UUID(season[1]).hex, "season": new_season.hex, "number": number, "episode": new_episode.hex,
            "files": [[episode_path, episode[1]]]}


def seed_gone_file_userdata(conn, manifest):
    """A movie whose file is not there and the play state two users recorded for it: what is left
    when a file is deleted while the server is off."""
    source = first(conn, 'SELECT b."Id" FROM "BaseItems" b WHERE b."Type" = ? AND ' + original("BaseItems", 'b."Id"') +
                   ' AND EXISTS (SELECT 1 FROM "UserData" u WHERE u."ItemId" = b."Id") ORDER BY b."Id" LIMIT 1',
                   (MOVIE_TYPE,), "movie with play state")
    ident = seed_id("messy-gone-file")
    key = "messy-gone-file-" + ident.hex
    clone_one(conn, "BaseItems", ('"Id" = ?', (source[0],)), {
        "Id": spelled(source[0], ident), "Name": "Vanished Movie", "PresentationUniqueKey": ident.hex,
        "Path": "/media/movies/Vanished Movie (2001)/Vanished Movie.mkv"}, "movie")
    # users the database has: another shape's rows of a deleted user are not this shape's to copy
    users = [user for (user,) in conn.execute(
        'SELECT DISTINCT d."UserId" FROM "UserData" d WHERE d."ItemId" = ? AND lower(replace(d."UserId", \'-\', \'\')) IN '
        '(SELECT lower(replace("Id", \'-\', \'\')) FROM "Users") ORDER BY d."UserId" LIMIT ?', (source[0], GONE_FILE_USERS))]
    # one row per user (a user can hold several under different keys, and the key here is one)
    rows = sum(clone_rows(conn, "UserData", '"ItemId" = ? AND "UserId" = ? ORDER BY "CustomDataKey"', (source[0], user),
                          {"ItemId": spelled(source[0], ident), "CustomDataKey": key, "Played": 1}, 1) for user in users)
    if not rows:
        raise CheckError("messy seed: the fixture has no play state to copy")
    return {"item": ident.hex, "key": key, "rows": rows}


def seed_non_ascii_names(conn, manifest):
    """Names that case and Unicode mappings disagree on: three users (each stored, where the table has
    the column, with a normalised name that is no normalisation of it, "stale-<label>" because the
    column is unique), one person with a decomposed name, and a movie whose path has an accent, a
    decomposed accent and a symbol. (The accented capital and small person pair belongs to case_duplicates.)"""
    has_normalized = any(column[1] == "NormalizedUsername" for column in conn.execute('PRAGMA table_info("Users")'))
    source = stored_user(conn, manifest)
    top = conn.execute('SELECT COALESCE(MAX("InternalId"), 0) FROM "Users"').fetchone()[0]
    users = {}
    for index, (label, name) in enumerate(UNICODE_USERS.items(), 1):
        ident = seed_id("messy-unicode-user-" + label)
        changes = {"Id": spelled(source, ident), "Username": name, "InternalId": top + index}
        if has_normalized:
            changes["NormalizedUsername"] = "stale-" + label
        clone_one(conn, "Users", ('"Id" = ?', (source,)), changes, "user")
        users[label] = {"user": ident.hex, "name": name}
    person = first(conn, 'SELECT "Id", "Path" FROM "BaseItems" WHERE "Type" = ? AND ' + original("BaseItems") + ' ORDER BY "Id" LIMIT 1',
                   (PERSON_TYPE,), "person")
    peoples = first(conn, 'SELECT "Id" FROM "Peoples" WHERE ' + original("Peoples") + ' ORDER BY "Id" LIMIT 1', (), "people row")
    people = {}
    for label in ("decomposed",):
        name = UNICODE_PEOPLE[label]
        item, row = seed_id("messy-unicode-person-" + label), seed_id("messy-unicode-people-" + label)
        clone_one(conn, "BaseItems", ('"Id" = ?', (person[0],)), {
            "Id": spelled(person[0], item), "Name": name, "CleanName": name.lower(), "SortName": name,
            "PresentationUniqueKey": item.hex, "Path": person_path(person[1], name)}, "person")
        clone_one(conn, "Peoples", ('"Id" = ?', (peoples[0],)), {"Id": spelled(peoples[0], row), "Name": name}, "people row")
        people[label] = {"item": item.hex, "people": row.hex, "name": name}
    movie = first(conn, 'SELECT "Id", "Path" FROM "BaseItems" WHERE lower(replace("Id", \'-\', \'\')) = ?',
                  (uuid.UUID(manifest["private_movie"]).hex,), "movie")
    ident = seed_id("messy-unicode-path")
    clone_one(conn, "BaseItems", ('"Id" = ?', (movie[0],)), {
        "Id": spelled(movie[0], ident), "Name": "Synthetic unicode path", "PresentationUniqueKey": ident.hex,
        "Path": UNICODE_PATH}, "movie")
    return {"users": users, "people": people, "movie": ident.hex, "path": UNICODE_PATH,
            "files": [[UNICODE_PATH, movie[1]]]}


SEEDERS = {"deleted_user": seed_deleted_user, "dangling_owner_extra": seed_dangling_owner_extra,
           "duplicate_paths": seed_duplicate_paths, "guid_casing": seed_guid_casing,
           "case_duplicates": seed_case_duplicates, "orphan_ancestor": seed_orphan_ancestor,
           "virtual_season": seed_virtual_season, "gone_file_userdata": seed_gone_file_userdata,
           "non_ascii_names": seed_non_ascii_names}


def original(table, column='"Id"'):
    """SQL limiting a query on `table` to the rows it held before any shape wrote (seed_messy keeps
    their ids in a temporary table): a shape copies from those only, never from another shape's rows,
    so the combined fixture ends as each shape alone does whatever the ids sort like."""
    return f'{column} IN (SELECT "Id" FROM temp."messy_original_{table}")'


def seed_messy(db, manifest, shapes=SHAPES):
    """Write `shapes` into the STOPPED database `db`; returns the manifest entry naming the rows."""
    entry = {"version": 1, "shapes": {}}
    with closing(sqlite3.connect(db)) as conn:
        # A deleted user and a missing owner need foreign keys off, outside a transaction.
        conn.execute("PRAGMA foreign_keys = OFF")
        with conn:
            for table in ("BaseItems", "Peoples"):
                conn.execute(f'CREATE TEMP TABLE "messy_original_{table}" AS SELECT "Id" FROM "{table}"')
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


def stored_hex(column):
    """SQL that reads the id column `column` as 32 lower-case hex digits, whatever its spelling."""
    return f"lower(replace(\"{column}\", '-', ''))"


def count_where(conn, table, where, params):
    return conn.execute(f'SELECT COUNT(*) FROM "{table}" WHERE {where}', params).fetchone()[0]


def ref_form(conn, value):
    """How an identifier column reads: null, or its spelling (lower36, upper36, mixed36, n32 lower or
    upper, other) and whether it names an item, the all-zero id, or nothing."""
    if value is None:
        return "null"
    try:
        parsed = uuid.UUID(value)
    except ValueError:
        return "invalid"
    if len(value) == 36:
        form = "lower36" if value == value.lower() else "upper36" if value == value.upper() else "mixed36"
    elif len(value) == 32:
        form = "n32lower" if value == value.lower() else "n32upper"
    else:
        form = "other"
    if parsed.int == 0:
        return form + "-zero"
    return form + ("->item" if item(conn, parsed.hex) else "->none")


def guid_casing_state(conn, entry):
    state = {}
    for label, ident in entry["items"].items():
        row = conn.execute(f'SELECT "OwnerId", "PrimaryVersionId" FROM "BaseItems" WHERE {stored_hex("Id")} = ?',
                           (ident,)).fetchone()
        state[label] = "gone" if row is None else {"owner": ref_form(conn, row[0]), "primary": ref_form(conn, row[1])}
    return state


def named(conn, table, kind, key, column="Type"):
    """The rows of `table` (of `column` `kind` when it has one) whose name lower-cases to `key`: the ids
    of those rows. Lower-cased here, not in SQL, whose lower() leaves an accented capital alone."""
    where, params = (f'"{column}" = ?', (kind,)) if kind else ("1 = 1", ())
    return [ident for ident, name in conn.execute(f'SELECT "Id", "Name" FROM "{table}" WHERE {where}', params)
            if name is not None and name.lower() == key]


def case_duplicates_state(conn, entry):
    """How many rows of each pair are left, and how many links a movie keeps to the people rows."""
    keys, movie = entry["keys"], entry["movie"]

    def links(people):
        return sum(count_where(conn, "PeopleBaseItemMap", f'{stored_hex("ItemId")} = ? AND "PeopleId" = ?', (movie, row))
                   for row in people)

    # The ASCII pair counts rows of its own person type: the same name may be credited with another
    # type too, which no merge folds into it (MergeDuplicatePeople groups by name and type).
    ascii_people = named(conn, "Peoples", keys["people_type"], keys["people_ascii"], column="PersonType")
    # Every type: both unicode rows are copies of one seeded row, and no other type bears the name.
    unicode_people = named(conn, "Peoples", None, keys["unicode"])
    return {"person_item_ascii": len(named(conn, "BaseItems", PERSON_TYPE, keys["person_item_ascii"])),
            "people_ascii": {"rows": len(ascii_people), "links": links(ascii_people)},
            "unicode": {"items": len(named(conn, "BaseItems", PERSON_TYPE, keys["unicode"])),
                        "peoples": len(unicode_people), "links": links(unicode_people)},
            "artists": len(named(conn, "BaseItems", ARTIST_TYPE, keys["artist"]))}


def orphan_ancestor_state(conn, entry):
    movie = stored_hex("ItemId")
    return {"missing_parent_rows": count_where(conn, "AncestorIds", f'{movie} = ? AND {stored_hex("ParentItemId")} = ?',
                                               (entry["movie"], entry["missing_parent"])),
            "missing_item_rows": count_where(conn, "AncestorIds", f"{movie} = ?", (entry["missing_item"],))}


def virtual_season_state(conn, entry):
    season = conn.execute(f'SELECT "Id", "Path" FROM "BaseItems" WHERE "Type" = ? AND {stored_hex("SeriesId")} = ? '
                          'AND "IndexNumber" = ?', (SEASON_TYPE, entry["series"], entry["number"])).fetchone()
    episode = conn.execute(f'SELECT "ParentId", "SeasonId" FROM "BaseItems" WHERE {stored_hex("Id")} = ?',
                           (entry["episode"],)).fetchone()
    return {"season": None if season is None else {"has_path": season[1] is not None},
            "episode": None if episode is None else {
                "under_season": season is not None and hex_of(episode[0]) == hex_of(season[0]),
                "season_id_is_season": season is not None and hex_of(episode[1]) == hex_of(season[0])}}


def gone_file_userdata_state(conn, entry):
    return {"item": item(conn, entry["item"]) is not None,
            "userdata_on_item": count_where(conn, "UserData", f'{stored_hex("ItemId")} = ?', (entry["item"],)),
            "userdata_with_key": count_where(conn, "UserData", '"CustomDataKey" = ?', (entry["key"],))}


def non_ascii_names_state(conn, entry):
    has_normalized = any(column[1] == "NormalizedUsername" for column in conn.execute('PRAGMA table_info("Users")'))
    users = {}
    for label, user in entry["users"].items():
        row = conn.execute('SELECT "NormalizedUsername" FROM "Users" WHERE "Username" = ?' if has_normalized else
                           'SELECT 1 FROM "Users" WHERE "Username" = ?', (user["name"],)).fetchone()
        users[label] = None if row is None else (row[0] if has_normalized else True)
    # people are counted by their exact stored name (byte for byte), not by id: a server may re-key a person
    people = {label: {"items": count_where(conn, "BaseItems", '"Type" = ? AND "Name" = ?', (PERSON_TYPE, row["name"])),
                      "peoples": count_where(conn, "Peoples", '"Name" = ?', (row["name"],))}
              for label, row in entry["people"].items()}
    path = conn.execute(f'SELECT "Path" FROM "BaseItems" WHERE {stored_hex("Id")} = ?', (entry["movie"],)).fetchone()
    return {"users": users, "people": people, "path": path and path[0]}


STATE_READERS = {"guid_casing": guid_casing_state, "case_duplicates": case_duplicates_state,
                 "orphan_ancestor": orphan_ancestor_state, "virtual_season": virtual_season_state,
                 "gone_file_userdata": gone_file_userdata_state, "non_ascii_names": non_ascii_names_state}


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
    for shape, reader in STATE_READERS.items():
        if shape in shapes:
            state[shape] = reader(conn, shapes[shape])
    return state


def read_state(db, entry):
    with closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as conn:
        return shape_state(conn, entry)


# ── what Ferrofin is expected to differ in ──────────────────────────────────
# (shape, lineage) -> (the state Ferrofin must reach, why it differs from Jellyfin 12.2). A
# difference from Jellyfin 12.2 that is not here fails the run. Read off the oracle's record
# (messy-oracle.json) and a live adoption.
NO_USER_ROWS = {table: 0 for table in USER_TABLES}
NO_ORPHAN_ANCESTORS = {"missing_parent_rows": 0, "missing_item_rows": 0}
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
    # refuses a dangling OwnerId, so migration 0045 hands the extra to the placeholder item and the
    # boot repair removes it, as on the 10.11 lineage, where 12.2 deletes it too.
    ("dangling_owner_extra", "12.x"): ({"exists": False, "owner": None},
                                       "Ferrofin removes a dangling-owner extra (as 12.2 does from 10.11); 12.2 keeps it on 12.x"),
    # A 12.x database has already run Jellyfin's FixIncorrectOwnerIdRelationships, so 12.2 keeps both
    # rows. Ferrofin runs its port once per adoption on any source and keeps the row that routine
    # keeps (children, then extras, then the newer one), deleting the other.
    ("duplicate_paths", "12.x"): ({"original": True, "duplicate": False},
                                  "Ferrofin collapses duplicate paths at adoption; 12.2 does not re-run its routine on 12.x"),
    # Jellyfin 12.2 leaves a 12.x database as written, because its upgrade routines ran
    # when that database was upgraded; Ferrofin runs its ports once, whatever the source.
    # 0032 (a 10.11 source) upper-cases an OwnerId and nulls one that is not 36 characters, as
    # Jellyfin's ChangeOwnerIdToGuid does; 0045 does both for every other source (an owner that
    # exists in another case takes the id's spelling, a 32-character one becomes NULL). PrimaryVersionId
    # is left as written, as 12.2 leaves it, though Ferrofin's own lookups of it are byte-exact: whether
    # it should be re-spelled too is gl-752, and this expectation changes with that decision.
    ("guid_casing", "12.x"): ({"owner_lower36": {"owner": "upper36->item", "primary": "null"},
                              "owner_n32": {"owner": "null", "primary": "null"},
                              "primary_lower36": {"owner": "null", "primary": "lower36->item"},
                              "primary_n32": {"owner": "null", "primary": "n32lower->item"}},
                             "0045 re-spells an owner in another case and nulls a 32-character one; 12.2 keeps them as written"),
    # MergeDuplicatePeople and MergeDuplicateMusicArtists (12.0) run once per adoption in Ferrofin, so a
    # case-only pair on a 12.x database is merged (people grouped by lower-cased name and type).
    ("case_duplicates", "12.x"): ({"person_item_ascii": 1, "people_ascii": {"rows": 1, "links": 1},
                                  "unicode": {"items": 1, "peoples": 1, "links": 1}, "artists": 1},
                                 "Ferrofin merges case-only duplicates at adoption; 12.2 does not re-run its routines on 12.x"),
    # Migration 0044 deletes AncestorIds rows naming an item that is not there (what the key's cascade
    # would have removed); Jellyfin 12.2 never looks at them and keeps both rows (Ferrofin's
    # every-boot foreign_key_check stays strict, so it cannot keep a row naming an item that is gone).
    ("orphan_ancestor", "10.11"): (NO_ORPHAN_ANCESTORS, "0044 deletes ancestor rows naming a missing item; Jellyfin 12.2 keeps them"),
    ("orphan_ancestor", "12.x"): (NO_ORPHAN_ANCESTORS, "0044 deletes ancestor rows naming a missing item; Jellyfin 12.2 keeps them"),
    # Ferrofin sets every user's NormalizedUsername to the name upper-cased as .NET's ToUpperInvariant
    # does (normalized_usernames.rs), once, whatever is stored; Jellyfin 12.2 recomputes it only when
    # it upgrades a 10.11 database and keeps the stored value on a 12.x one.
    ("non_ascii_names", "12.x"): ({"users": {"decomposed": "ZOE\u0308", "dotted_capital_i": "\u0130STANBUL", "sharp_s": "STRA\u00dfE"},
                                  "people": {"decomposed": {"items": 1, "peoples": 1}}, "path": UNICODE_PATH},
                                 "Ferrofin recomputes every user's normalised name at adoption; 12.2 keeps the stored value on 12.x"),
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
            outcome = run_oracle_server(copy, root / MESSY_MEDIA, port)
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
        detail = "; ".join(f"{shape}: alone {record['shapes'][shape]['state'][shape]}, with all {record['combined']['state'][shape]}"
                           for shape in differing)
        raise CheckError("Jellyfin 12.2 handled " + ", ".join(differing) + " differently with all the shapes "
                         f"present than with each alone ({detail}); the per-shape record would be the wrong expectation")
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
            link_media(root, entry)
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
