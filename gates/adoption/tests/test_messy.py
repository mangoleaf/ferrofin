"""The messy-row shapes on a scratch SQLite database: each seeder, the end-state reader, and the
comparison against Jellyfin 12.2's record and the written divergences (no docker, no fixtures)."""

import contextlib
from contextlib import closing
import io
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unicodedata
import unittest
from unittest import mock
import uuid

sys.path.insert(0, str(Path(__file__).parents[1]))
import messy
from metadata import CheckError
from synthetic_fixture import USERS, seed_id

ADULT = "5136F4F7-6903-1423-B8DF-4F11C37566A5"
ITEM = "1559654E-1FB0-5780-0C66-00CA97F14DBB"
OTHER_ITEM = "2A59654E-1FB0-5780-0C66-00CA97F14DBB"
EXTRA = "3B36F4F7-6903-1423-B8DF-4F11C37566A5"
PRIVATE = "4C36F4F7-6903-1423-B8DF-4F11C37566A5"
PERSON = "5D36F4F7-6903-1423-B8DF-4F11C37566A5"
ARTIST = "6E36F4F7-6903-1423-B8DF-4F11C37566A5"
SERIES = "7F36F4F7-6903-1423-B8DF-4F11C37566A5"
SEASON = "8036F4F7-6903-1423-B8DF-4F11C37566A5"
EPISODE = "9136F4F7-6903-1423-B8DF-4F11C37566A5"
PEOPLE = "A236F4F7-6903-1423-B8DF-4F11C37566A5"
SCHEMA = """
CREATE TABLE Users (Id TEXT NOT NULL PRIMARY KEY, Username TEXT NOT NULL, NormalizedUsername TEXT NOT NULL DEFAULT '',
    InternalId INTEGER NOT NULL);
CREATE UNIQUE INDEX IX_Users_NormalizedUsername ON Users (NormalizedUsername);
CREATE TABLE BaseItems (Id TEXT NOT NULL PRIMARY KEY, Type TEXT NOT NULL, Name TEXT, Path TEXT, OwnerId TEXT,
    PrimaryVersionId TEXT, ExtraType INTEGER NULL, PresentationUniqueKey TEXT, DateCreated TEXT, CleanName TEXT,
    SortName TEXT, IndexNumber INTEGER, ParentIndexNumber INTEGER, ParentId TEXT, SeasonId TEXT, SeriesId TEXT);
CREATE TABLE Peoples (Id TEXT NOT NULL PRIMARY KEY, Name TEXT NOT NULL, PersonType TEXT);
CREATE TABLE PeopleBaseItemMap (ItemId TEXT NOT NULL, PeopleId TEXT NOT NULL, Role TEXT NOT NULL, ListOrder INTEGER,
    SortOrder INTEGER, PRIMARY KEY (ItemId, PeopleId, Role));
CREATE TABLE AncestorIds (ItemId TEXT NOT NULL, ParentItemId TEXT NOT NULL, PRIMARY KEY (ItemId, ParentItemId));
CREATE TABLE UserData (ItemId TEXT NOT NULL, UserId TEXT NOT NULL, CustomDataKey TEXT NOT NULL,
    Played INTEGER NOT NULL, PRIMARY KEY (ItemId, UserId, CustomDataKey));
CREATE TABLE Permissions (Id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, Kind INTEGER NOT NULL,
    RowVersion INTEGER NOT NULL, UserId TEXT NOT NULL, Value INTEGER NOT NULL);
CREATE UNIQUE INDEX IX_Permissions_UserId_Kind ON Permissions (UserId, Kind);
CREATE TABLE Preferences (Id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, Kind INTEGER NOT NULL,
    RowVersion INTEGER NOT NULL, UserId TEXT NOT NULL, Value TEXT NOT NULL);
CREATE TABLE Devices (Id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, UserId TEXT NOT NULL, AccessToken TEXT NOT NULL,
    DeviceId TEXT NOT NULL);
CREATE TABLE DisplayPreferences (Id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, UserId TEXT NOT NULL,
    Client TEXT NOT NULL, ShowSidebar INTEGER NOT NULL DEFAULT 0, ShowBackdrop INTEGER NOT NULL DEFAULT 0,
    ScrollDirection INTEGER NOT NULL DEFAULT 0, SkipForwardLength INTEGER NOT NULL DEFAULT 0,
    SkipBackwardLength INTEGER NOT NULL DEFAULT 0, ChromecastVersion INTEGER NOT NULL DEFAULT 0,
    EnableNextVideoInfoOverlay INTEGER NOT NULL DEFAULT 0,
    ItemId TEXT NOT NULL);
CREATE UNIQUE INDEX IX_DisplayPreferences ON DisplayPreferences (UserId, ItemId, Client);
"""
MANIFEST = {"users": {name: str(uuid.UUID(ADULT)) for name in USERS}, "private_movie": PRIVATE}


def populate(db):
    """A scratch database holding the rows the seeders copy: one user, two movies, an extra."""
    with closing(sqlite3.connect(db)) as conn:
        conn.executescript(SCHEMA + f"""
            INSERT INTO Users (Id, Username, NormalizedUsername, InternalId) VALUES ('{ADULT}', 'adult', 'ADULT', 1);
            INSERT INTO BaseItems (Id, Type, Name, Path, OwnerId, ExtraType, PresentationUniqueKey, DateCreated)
              VALUES ('{ITEM}', 'MediaBrowser.Controller.Entities.Movies.Movie', 'Movie', '/media/movies/a/a.mkv', NULL, NULL, 'k1', '2026-10-20 04:22:14.4161545');
            INSERT INTO BaseItems (Id, Type, Name, Path, OwnerId, ExtraType, PresentationUniqueKey, DateCreated)
              VALUES ('{PRIVATE}', 'MediaBrowser.Controller.Entities.Movies.Movie', 'Private', '/media/movies/p/p.mkv', NULL, NULL, 'k2', '2026-10-20 04:22:15.4161545');
            INSERT INTO BaseItems (Id, Type, Name, Path, OwnerId, ExtraType, PresentationUniqueKey, DateCreated)
              VALUES ('{EXTRA}', '{messy.VIDEO_TYPE}', 'Making of', '/media/movies/a/extras/x.mkv', '{ITEM.lower()}', 3, 'k3', '2026-10-20 04:22:16.4161545');
            INSERT INTO BaseItems (Id, Type, Name, CleanName, SortName, Path, PresentationUniqueKey)
              VALUES ('{PERSON}', '{messy.PERSON_TYPE}', 'Synthetic Actor', 'synthetic actor', 'Synthetic Actor',
                      '%MetadataPath%/People/S/Synthetic Actor', 'k4');
            INSERT INTO BaseItems (Id, Type, Name, CleanName, SortName, Path, PresentationUniqueKey)
              VALUES ('{ARTIST}', '{messy.ARTIST_TYPE}', 'Synthetic Soloist', 'synthetic soloist', 'Synthetic Soloist', '/media/music/Synthetic Soloist', 'k5');
            INSERT INTO BaseItems (Id, Type, Name, Path, PresentationUniqueKey) VALUES ('{SERIES}', 'MediaBrowser.Controller.Entities.TV.Series', 'Show', '/media/shows/Show', 'k6');
            INSERT INTO BaseItems (Id, Type, Name, Path, IndexNumber, ParentId, SeriesId, PresentationUniqueKey)
              VALUES ('{SEASON}', '{messy.SEASON_TYPE}', 'Season 1', '/media/shows/Show/Season 01', 1, '{SERIES}', '{SERIES}', 'k7');
            INSERT INTO BaseItems (Id, Type, Name, Path, IndexNumber, ParentIndexNumber, ParentId, SeasonId, SeriesId, PresentationUniqueKey)
              VALUES ('{EPISODE}', '{messy.EPISODE_TYPE}', 'Episode', '/media/shows/Show/Season 01/e1.mkv', 1, 1, '{SEASON}', '{SEASON}', '{SERIES}', 'k8');
            INSERT INTO Peoples VALUES ('{PEOPLE}', 'Synthetic Actor', 'Actor');
            INSERT INTO PeopleBaseItemMap VALUES ('{ITEM}', '{PEOPLE}', 'Lead', 0, 0);
            INSERT INTO UserData VALUES ('{ITEM}', '{ADULT}', 'a', 1), ('{PRIVATE}', '{ADULT}', 'b', 0), ('{EXTRA}', '{ADULT}', 'c', 0);
            INSERT INTO Permissions (Kind, RowVersion, UserId, Value) VALUES (0, 0, '{ADULT}', 1), (1, 0, '{ADULT}', 0);
            INSERT INTO Preferences (Kind, RowVersion, UserId, Value) VALUES (2, 0, '{ADULT}', 'x');
            INSERT INTO Devices (UserId, AccessToken, DeviceId) VALUES ('{ADULT}', 'tok', 'dev');
            INSERT INTO DisplayPreferences (UserId, Client, ItemId) VALUES ('{ADULT}', 'emby', '00000000-0000-0000-0000-000000000000');
        """)


class Scratch(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.db = str(Path(directory.name) / "jellyfin.db")
        populate(self.db)

    def conn(self):
        conn = sqlite3.connect(self.db)
        self.addCleanup(conn.close)
        return conn


class Seeders(Scratch):
    def test_deleted_user_rows_name_a_user_that_is_not_there(self):
        entry = messy.seed_messy(self.db, MANIFEST, ("deleted_user",))["shapes"]["deleted_user"]
        ghost = entry["user"]
        self.assertEqual(entry["rows"], {"UserData": 2, "Permissions": 2, "Preferences": 1, "Devices": 1, "DisplayPreferences": 1})
        conn = self.conn()
        self.assertEqual(conn.execute("SELECT COUNT(*) FROM Users WHERE lower(replace(Id, '-', '')) = ?", (ghost,)).fetchone()[0], 0)
        state = messy.shape_state(conn, {"shapes": {"deleted_user": entry}})["deleted_user"]
        self.assertEqual(state, entry["rows"])
        # the cloned rows are written in the stored spelling (dashed, upper case)
        self.assertEqual(conn.execute("SELECT UserId FROM Devices WHERE DeviceId = 'synthetic-messy-deleted-device'").fetchone()[0],
                         str(uuid.UUID(ghost)).upper())

    def test_the_account_the_rows_were_copied_from_is_untouched(self):
        messy.seed_messy(self.db, MANIFEST, ("deleted_user",))
        conn = self.conn()
        self.assertEqual(conn.execute("SELECT COUNT(*) FROM Permissions WHERE UserId = ?", (ADULT,)).fetchone()[0], 2)
        self.assertEqual(conn.execute("SELECT COUNT(*) FROM UserData WHERE UserId = ?", (ADULT,)).fetchone()[0], 3)

    def test_nothing_to_copy_is_a_build_failure_not_a_silent_skip(self):
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute("DELETE FROM Devices")
        with self.assertRaisesRegex(CheckError, "nothing to copy for Devices"):
            messy.seed_messy(self.db, MANIFEST, ("deleted_user",))

    def test_a_fixture_with_no_display_preferences_gets_one_written_for_the_deleted_user(self):
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute("DELETE FROM DisplayPreferences")
        entry = messy.seed_messy(self.db, MANIFEST, ("deleted_user",))["shapes"]["deleted_user"]
        self.assertEqual(entry["rows"]["DisplayPreferences"], 1)
        row = self.conn().execute("SELECT UserId, Client, ItemId FROM DisplayPreferences").fetchone()
        self.assertEqual((uuid.UUID(row[0]).hex, row[1]), (entry["user"], "synthetic-messy"))
        # a user-wide preference: the empty Guid, in the spelling the other ids use
        self.assertEqual(row[2], str(uuid.UUID(int=0)).upper())

    def test_dangling_owner_extra_names_an_owner_no_row_has(self):
        entry = messy.seed_messy(self.db, MANIFEST, ("dangling_owner_extra",))["shapes"]["dangling_owner_extra"]
        conn = self.conn()
        row = conn.execute("SELECT Id, OwnerId, ExtraType, Path FROM BaseItems WHERE lower(replace(Id, '-', '')) = ?",
                           (entry["extra"],)).fetchone()
        owner = uuid.UUID(row[1]).hex
        self.assertEqual(owner, entry["owner"])
        self.assertEqual(conn.execute("SELECT COUNT(*) FROM BaseItems WHERE lower(replace(Id, '-', '')) = ?", (owner,)).fetchone()[0], 0)
        self.assertEqual(row[2], 3)
        self.assertEqual(messy.shape_state(conn, {"shapes": {"dangling_owner_extra": entry}})["dangling_owner_extra"],
                         {"exists": True, "owner": "dangling"})

    def test_duplicate_paths_share_a_path_and_the_copy_is_the_older_row(self):
        entry = messy.seed_messy(self.db, MANIFEST, ("duplicate_paths",))["shapes"]["duplicate_paths"]
        conn = self.conn()
        rows = conn.execute("SELECT Id, DateCreated FROM BaseItems WHERE Path = ? ORDER BY DateCreated", (entry["path"],)).fetchall()
        self.assertEqual(len(rows), 2)
        self.assertEqual(uuid.UUID(rows[0][0]).hex, entry["duplicate"])
        self.assertEqual(uuid.UUID(rows[1][0]).hex, entry["original"])
        self.assertEqual(messy.shape_state(conn, {"shapes": {"duplicate_paths": entry}})["duplicate_paths"],
                         {"original": True, "duplicate": True})

    def test_the_manifest_names_every_shape_and_all_shapes_can_share_one_database(self):
        entry = messy.seed_messy(self.db, MANIFEST)
        self.assertEqual(tuple(entry["shapes"]), messy.SHAPES)

    def test_ids_are_fixed_so_a_rebuild_writes_the_same_rows(self):
        first = messy.seed_messy(self.db, MANIFEST, ("dangling_owner_extra", "duplicate_paths"))
        self.assertEqual(first["shapes"]["duplicate_paths"]["duplicate"], seed_id("messy-duplicate").hex)
        self.assertEqual(first["shapes"]["dangling_owner_extra"]["extra"], seed_id("messy-extra").hex)


class NewShapeSeeders(Scratch):
    """The six shapes of identifier spelling, duplicates and names: what each writes and how its end
    state reads, on the scratch database."""

    def seed(self, shape):
        entry = messy.seed_messy(self.db, MANIFEST, (shape,))["shapes"][shape]
        return entry, {"shapes": {shape: entry}}

    def test_ref_form_reads_every_spelling_and_whether_it_names_an_item(self):
        conn = self.conn()
        lower, upper = str(uuid.UUID(ITEM)).lower(), str(uuid.UUID(ITEM)).upper()
        for value, form in ((None, "null"), (lower, "lower36->item"), (upper, "upper36->item"),
                            (lower[:8] + upper[8:], "mixed36->item"), (uuid.UUID(ITEM).hex, "n32lower->item"),
                            (uuid.UUID(ITEM).hex.upper(), "n32upper->item"), (str(uuid.UUID(int=0)), "lower36-zero"),
                            ("00000000000000000000000000000000", "n32lower-zero"), ("not a guid", "invalid"),
                            (str(uuid.UUID(int=0xabc)).upper(), "upper36->none"), (uuid.UUID(int=0xabc).hex, "n32lower->none")):
            with self.subTest(form):
                self.assertEqual(messy.ref_form(conn, value), form)

    def test_guid_casing_writes_two_extras_and_two_versions_naming_existing_items_in_two_spellings(self):
        entry, wanted = self.seed("guid_casing")
        self.assertEqual(set(entry["items"]), {"owner_lower36", "owner_n32", "primary_lower36", "primary_n32"})
        self.assertEqual(messy.read_state(self.db, wanted)["guid_casing"], {
            "owner_lower36": {"owner": "lower36->item", "primary": "null"},
            "owner_n32": {"owner": "n32lower->item", "primary": "null"},
            "primary_lower36": {"owner": "null", "primary": "lower36->item"},
            "primary_n32": {"owner": "null", "primary": "n32lower->item"}})
        conn = self.conn()
        paths = [path for (path,) in conn.execute("SELECT Path FROM BaseItems WHERE Name LIKE 'Synthetic guid casing%'")]
        self.assertEqual(len(set(paths)), 4)   # no two share a path, or the duplicate-path repair would fold them
        self.assertEqual(conn.execute("SELECT COUNT(*) FROM BaseItems WHERE Name LIKE 'Synthetic guid casing%' AND ExtraType = 3").fetchone()[0], 2)

    def test_guid_casing_needs_an_extra_with_an_owner_to_copy(self):
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute("DELETE FROM BaseItems WHERE ExtraType IS NOT NULL")
        with self.assertRaisesRegex(CheckError, "no extra with an owner to copy"):
            messy.seed_messy(self.db, MANIFEST, ("guid_casing",))

    def test_case_duplicates_write_independent_pairs_that_differ_only_by_case(self):
        entry, wanted = self.seed("case_duplicates")
        conn = self.conn()
        # the pairs: a person item, a Peoples row (linked to a movie), and an accented pair of each
        self.assertEqual(sorted(name for (name,) in conn.execute("SELECT Name FROM BaseItems WHERE Type = ?", (messy.PERSON_TYPE,))),
                         sorted(["Synthetic Actor", "SYNTHETIC ACTOR", messy.UNICODE_PEOPLE["capital"], messy.UNICODE_PEOPLE["small"]]))
        self.assertEqual(sorted(name for (name,) in conn.execute("SELECT Name FROM Peoples")),
                         sorted(["Synthetic Actor", "SYNTHETIC ACTOR", messy.UNICODE_PEOPLE["capital"], messy.UNICODE_PEOPLE["small"]]))
        self.assertEqual(messy.UNICODE_PEOPLE["capital"].lower(), messy.UNICODE_PEOPLE["small"])
        self.assertEqual(messy.read_state(self.db, wanted)["case_duplicates"], {
            "person_item_ascii": 2, "people_ascii": {"rows": 2, "links": 2},
            "unicode": {"items": 2, "peoples": 2, "links": 2}, "artists": 2})
        self.assertEqual(conn.execute("SELECT Path FROM BaseItems WHERE Name = 'SYNTHETIC SOLOIST'").fetchone()[0],
                         "/media/music/SYNTHETIC SOLOIST")

    def test_the_people_pair_counts_rows_of_its_own_type(self):
        # The same name credited with another type is no part of the pair: no merge folds it in,
        # so which Peoples row the seed copies cannot change what a merge leaves.
        entry, wanted = self.seed("case_duplicates")
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute("INSERT INTO Peoples (Id, Name, PersonType) VALUES (?, 'Synthetic Actor', 'Director')",
                         (str(uuid.uuid4()).upper(),))
        self.assertEqual(messy.read_state(self.db, wanted)["case_duplicates"]["people_ascii"], {"rows": 2, "links": 2})

    def test_the_people_row_copied_is_the_first_by_name_not_by_id(self):
        # Jellyfin gives Peoples rows random ids: a linked row whose name sorts first and whose id
        # sorts last is the one copied, so every build of the fixture seeds the same person.
        first_by_name = "FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF"
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute("INSERT INTO Peoples VALUES (?, 'Aardvark Actor', 'Actor')", (first_by_name,))
            conn.execute("INSERT INTO PeopleBaseItemMap VALUES (?, ?, 'Support', 1, 1)", (ITEM, first_by_name))
        self.seed("case_duplicates")
        names = {name for (name,) in self.conn().execute("SELECT Name FROM Peoples")}
        self.assertIn("AARDVARK ACTOR", names)
        self.assertNotIn("SYNTHETIC ACTOR", names)

    def test_the_pairs_are_read_by_name_so_a_re_keyed_row_still_counts(self):
        entry, wanted = self.seed("case_duplicates")
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute("UPDATE BaseItems SET Id = ? WHERE Name = 'SYNTHETIC ACTOR'", (str(uuid.UUID(int=0xabc)).upper(),))
            conn.execute("UPDATE Peoples SET Id = ? WHERE Name = 'SYNTHETIC ACTOR'", (str(uuid.UUID(int=0xdef)).upper(),))
        state = messy.read_state(self.db, wanted)["case_duplicates"]
        self.assertEqual((state["person_item_ascii"], state["people_ascii"]["rows"]), (2, 2))
        # the link still names the old people id, so it no longer counts: a re-keyed row without its link shows
        self.assertEqual(state["people_ascii"]["links"], 1)

    def test_no_two_persons_share_a_metadata_path_so_the_duplicate_path_repair_cannot_stand_in_for_the_merge(self):
        self.seed("case_duplicates")
        paths = [path for (path,) in self.conn().execute("SELECT Path FROM BaseItems WHERE Type = ?", (messy.PERSON_TYPE,))]
        self.assertEqual(len(paths), len(set(paths)))
        self.assertIn("%MetadataPath%/People/S/SYNTHETIC ACTOR", paths)
        self.assertIn(f"%MetadataPath%/People/{messy.UNICODE_PEOPLE['small'][0].upper()}/{messy.UNICODE_PEOPLE['small']}", paths)

    def test_case_duplicates_state_shows_a_merge(self):
        entry, wanted = self.seed("case_duplicates")
        with closing(sqlite3.connect(self.db)) as conn, conn:
            # what a merge does: the upper-case rows go, and the link of the duplicate people row with them
            conn.execute("DELETE FROM BaseItems WHERE Name = 'SYNTHETIC ACTOR'")
            conn.execute("DELETE FROM PeopleBaseItemMap WHERE PeopleId IN (SELECT Id FROM Peoples WHERE Name = 'SYNTHETIC ACTOR')")
            conn.execute("DELETE FROM Peoples WHERE Name = 'SYNTHETIC ACTOR'")
        state = messy.read_state(self.db, wanted)["case_duplicates"]
        self.assertEqual((state["person_item_ascii"], state["people_ascii"]), (1, {"rows": 1, "links": 1}))
        self.assertEqual(state["unicode"], {"items": 2, "peoples": 2, "links": 2})   # the other pair is untouched

    def test_orphan_ancestor_writes_a_row_with_a_missing_parent_and_one_with_a_missing_item(self):
        entry, wanted = self.seed("orphan_ancestor")
        self.assertEqual(messy.read_state(self.db, wanted)["orphan_ancestor"], {"missing_parent_rows": 1, "missing_item_rows": 1})
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute("DELETE FROM AncestorIds")
        self.assertEqual(messy.read_state(self.db, wanted)["orphan_ancestor"], {"missing_parent_rows": 0, "missing_item_rows": 0})

    def test_virtual_season_is_a_season_with_no_folder_one_past_the_highest_and_an_episode_in_it(self):
        entry, wanted = self.seed("virtual_season")
        self.assertEqual(entry["number"], 2)
        conn = self.conn()
        season = conn.execute("SELECT Path, IndexNumber, ParentId FROM BaseItems WHERE Name = 'Season 2'").fetchone()
        self.assertEqual((season[0], season[1], uuid.UUID(season[2]).hex), (None, 2, uuid.UUID(SERIES).hex))
        self.assertEqual(messy.read_state(self.db, wanted)["virtual_season"],
                         {"season": {"has_path": False}, "episode": {"under_season": True, "season_id_is_season": True}})
        self.assertEqual(conn.execute("SELECT Path FROM BaseItems WHERE Name = 'Synthetic virtual season episode'").fetchone()[0],
                         "/media/shows/Show/Synthetic virtual season episode S02E01.mkv")   # in the series folder: no Season folder to scan

    def test_virtual_season_state_follows_the_season_when_it_is_given_a_new_id(self):
        entry, wanted = self.seed("virtual_season")
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute("UPDATE BaseItems SET Id = ? WHERE Name = 'Season 2'", (sid := str(uuid.UUID(int=99)).upper(),))
            conn.execute("UPDATE BaseItems SET ParentId = ?, SeasonId = ? WHERE Name = 'Synthetic virtual season episode'", (sid, sid))
        self.assertEqual(messy.read_state(self.db, wanted)["virtual_season"]["episode"], {"under_season": True, "season_id_is_season": True})
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute("DELETE FROM BaseItems WHERE Name = 'Season 2'")
        self.assertEqual(messy.read_state(self.db, wanted)["virtual_season"], {"season": None, "episode": {"under_season": False, "season_id_is_season": False}})

    def test_gone_file_userdata_is_a_movie_with_no_file_and_the_play_state_recorded_for_it(self):
        entry, wanted = self.seed("gone_file_userdata")
        self.assertEqual(entry["rows"], 1)
        self.assertEqual(messy.read_state(self.db, wanted)["gone_file_userdata"],
                         {"item": True, "userdata_on_item": 1, "userdata_with_key": 1})
        self.assertEqual(self.conn().execute("SELECT Played FROM UserData WHERE CustomDataKey = ?", (entry["key"],)).fetchone()[0], 1)
        with closing(sqlite3.connect(self.db)) as conn, conn:
            # what a scan does to an item whose file is gone: the item goes, its state moves to the placeholder
            conn.execute("DELETE FROM BaseItems WHERE lower(replace(Id, '-', '')) = ?", (entry["item"],))
            conn.execute("UPDATE UserData SET ItemId = ? WHERE CustomDataKey = ?", (messy.PLACEHOLDER, entry["key"]))
        self.assertEqual(messy.read_state(self.db, wanted)["gone_file_userdata"],
                         {"item": False, "userdata_on_item": 0, "userdata_with_key": 1})

    def test_non_ascii_names_write_three_users_one_person_and_a_path_no_ascii_code_survives(self):
        entry, wanted = self.seed("non_ascii_names")
        conn = self.conn()
        self.assertEqual(sorted(name for (name,) in conn.execute("SELECT Username FROM Users WHERE Username <> 'adult'")),
                         sorted(messy.UNICODE_USERS.values()))
        # each normalised name is a distinct stale value (the column is unique), none a normalisation
        self.assertEqual(sorted(value for (value,) in conn.execute("SELECT NormalizedUsername FROM Users WHERE Username <> 'adult'")),
                         sorted("stale-" + label for label in messy.UNICODE_USERS))
        state = messy.read_state(self.db, wanted)["non_ascii_names"]
        self.assertEqual(state["users"], {label: "stale-" + label for label in messy.UNICODE_USERS})
        self.assertEqual(state["people"]["decomposed"], {"items": 1, "peoples": 1})
        self.assertEqual(state["path"], messy.UNICODE_PATH)
        # every decomposed text is one no normalisation has composed, so a mapping that normalises shows
        for text in (messy.UNICODE_USERS["decomposed"], messy.UNICODE_PEOPLE["decomposed"], messy.UNICODE_PATH):
            self.assertNotEqual(text, unicodedata.normalize("NFC", text))

    def test_the_users_are_copies_with_internal_ids_of_their_own(self):
        self.seed("non_ascii_names")
        ids = [value for (value,) in self.conn().execute("SELECT InternalId FROM Users ORDER BY InternalId")]
        self.assertEqual(ids, [1, 2, 3, 4])

    def test_every_new_shape_has_a_seeder_and_a_state_reader(self):
        for shape in messy.SHAPES:
            with self.subTest(shape):
                self.assertIn(shape, messy.SEEDERS)
        self.assertEqual(set(messy.STATE_READERS), set(messy.SHAPES) - {"deleted_user", "dangling_owner_extra", "duplicate_paths"})


class Independence(unittest.TestCase):
    """Each shape reads the same alone as with every other shape seeded, whatever the ids of the
    rows it copies sort like: it copies only rows the fixture had before any shape wrote."""

    NAMES = ("ITEM", "EXTRA", "PRIVATE", "PERSON", "ARTIST", "SERIES", "SEASON", "EPISODE", "PEOPLE")

    def late_ids(self):
        """Every scratch id moved behind the seeded ones (FFFF prefix, an index to keep them distinct)."""
        return {name: f"FFFF{index:04X}" + globals()[name][8:] for index, name in enumerate(self.NAMES)}

    def states(self, ids):
        """{shape: its end state alone} and the same with all shapes seeded, on scratch databases whose
        original rows carry `ids` (constant name -> id)."""
        module = sys.modules[__name__]
        with contextlib.ExitStack() as stack:
            if ids:
                stack.enter_context(mock.patch.multiple(module, **ids))
            stack.enter_context(mock.patch.dict(MANIFEST, {"private_movie": ids.get("PRIVATE", PRIVATE)}))
            directory = tempfile.TemporaryDirectory()
            self.addCleanup(directory.cleanup)

            def read(shapes, label):
                db = str(Path(directory.name) / f"{label}.db")
                populate(db)
                return messy.read_state(db, messy.seed_messy(db, MANIFEST, shapes))

            return {shape: read((shape,), shape)[shape] for shape in messy.SHAPES}, read(messy.SHAPES, "all")

    def assert_independent(self, ids):
        alone, together = self.states(ids)
        for shape in messy.SHAPES:
            with self.subTest(shape):
                self.assertEqual(alone[shape], together[shape])

    def test_with_the_original_ids_as_they_are(self):
        self.assert_independent({})

    def test_with_original_ids_that_sort_after_every_seeded_id(self):
        # a shape that picked "the first row" over the whole table would now pick another shape's row
        self.assert_independent(self.late_ids())

    def test_every_row_a_seeder_copies_from_is_one_the_fixture_held_before_any_shape_wrote(self):
        """Wraps what the seeders copy through (`first` picks, `clone_rows` copies) and checks every
        source id against the rows present before seeding, with ids that sort behind the seeded ones."""
        sources = []
        real_first, real_clone = messy.first, messy.clone_rows

        def first(conn, sql, params, what):
            row = real_first(conn, sql, params, what)
            sources.append((what, str(row[0]).replace("-", "").lower()))
            return row

        def clone(conn, table, where, params, changes, limit=None):
            if where == '"Id" = ?':
                sources.append((table, str(params[0]).replace("-", "").lower()))
            return real_clone(conn, table, where, params, changes, limit)

        ids = self.late_ids()
        with contextlib.ExitStack() as stack:
            stack.enter_context(mock.patch.multiple(sys.modules[__name__], **ids))
            stack.enter_context(mock.patch.dict(MANIFEST, {"private_movie": ids["PRIVATE"]}))
            stack.enter_context(mock.patch.object(messy, "first", first))
            stack.enter_context(mock.patch.object(messy, "clone_rows", clone))
            with tempfile.TemporaryDirectory() as directory:
                db = str(Path(directory) / "jellyfin.db")
                populate(db)
                with closing(sqlite3.connect(db)) as conn:
                    before = {str(row[0]).replace("-", "").lower()
                              for table in ("BaseItems", "Peoples", "Users") for row in conn.execute(f'SELECT "Id" FROM "{table}"')}
                messy.seed_messy(db, MANIFEST)
        self.assertGreater(len(sources), 10)   # the wrappers saw the seeders at work
        self.assertEqual([source for source in sources if source[1] not in before], [])

    def test_original_names_only_the_rows_present_before_any_shape_wrote(self):
        seen = {}

        def writes(conn, manifest):
            messy.clone_one(conn, "BaseItems", ('"Id" = ?', (ITEM,)), {"Id": "AAAAAAAA-0000-0000-0000-000000000001"}, "item")
            return {}

        def reads(conn, manifest):
            seen["ids"] = {row[0] for row in conn.execute(f'SELECT "Id" FROM "BaseItems" WHERE {messy.original("BaseItems")}')}
            seen["all"] = {row[0] for row in conn.execute('SELECT "Id" FROM "BaseItems"')}
            return {}

        with tempfile.TemporaryDirectory() as directory:
            db = str(Path(directory) / "jellyfin.db")
            populate(db)
            with closing(sqlite3.connect(db)) as conn:
                before = {row[0] for row in conn.execute('SELECT "Id" FROM "BaseItems"')}
            with mock.patch.dict(messy.SEEDERS, {"writes": writes, "reads": reads}):
                messy.seed_messy(db, MANIFEST, ("writes", "reads"))
        self.assertEqual(seen["ids"], before)
        self.assertEqual(len(seen["all"]), len(before) + 1)   # the row the first shape wrote is there, and not "original"


class EndState(Scratch):
    def test_each_way_an_extras_owner_can_read(self):
        entry = messy.seed_messy(self.db, MANIFEST, ("dangling_owner_extra",))["shapes"]["dangling_owner_extra"]
        wanted = {"shapes": {"dangling_owner_extra": entry}}
        extra = "lower(replace(Id, '-', '')) = '" + entry["extra"] + "'"
        for owner, kind in ((None, "none"), (f"'{ITEM}'", "other"), (f"'{messy.PLACEHOLDER}'", "placeholder"),
                            (f"'{str(uuid.UUID(entry['extra'])).upper()}'", "self")):
            with closing(sqlite3.connect(self.db)) as conn, conn:
                conn.execute(f"UPDATE BaseItems SET OwnerId = {owner or 'NULL'} WHERE {extra}")
            with self.subTest(kind):
                self.assertEqual(messy.read_state(self.db, wanted)["dangling_owner_extra"]["owner"], kind)
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.execute(f"DELETE FROM BaseItems WHERE {extra}")
        self.assertEqual(messy.read_state(self.db, wanted)["dangling_owner_extra"], {"exists": False, "owner": None})

    def test_rows_deleted_by_an_adoption_show_in_the_state(self):
        entry = messy.seed_messy(self.db, MANIFEST)
        ghost = entry["shapes"]["deleted_user"]["user"]
        with closing(sqlite3.connect(self.db)) as conn, conn:
            for table in messy.USER_TABLES:
                conn.execute(f"DELETE FROM {table} WHERE lower(replace(UserId, '-', '')) = ?", (ghost,))
            conn.execute("DELETE FROM BaseItems WHERE lower(replace(Id, '-', '')) = ?", (entry["shapes"]["duplicate_paths"]["duplicate"],))
        state = messy.read_state(self.db, entry)
        self.assertEqual(state["deleted_user"], {table: 0 for table in messy.USER_TABLES})
        self.assertEqual(state["duplicate_paths"], {"original": True, "duplicate": False})


class Comparison(unittest.TestCase):
    ENTRY = {"shapes": {"deleted_user": {}, "duplicate_paths": {}}}
    GONE = {table: 0 for table in messy.USER_TABLES}
    KEPT = {table: 1 for table in messy.USER_TABLES}

    def setUp(self):
        # These test the mechanism: the real divergences (WrittenDivergences) must not leak in.
        patcher = mock.patch.dict(messy.DIVERGENCES, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)

    def oracle(self, **shapes):
        return {"shapes": {name: record for name, record in shapes.items()}}

    def booted(self, shape, state):
        return {"outcome": "booted", "state": {shape: state}}

    def test_the_state_jellyfin_12_2_left_is_the_expectation(self):
        oracle = self.oracle(deleted_user=self.booted("deleted_user", self.GONE),
                             duplicate_paths=self.booted("duplicate_paths", {"original": True, "duplicate": False}))
        good = {"deleted_user": self.GONE, "duplicate_paths": {"original": True, "duplicate": False}}
        self.assertEqual(messy.compare(good, self.ENTRY, oracle, "10.11"), [])
        bad = {**good, "deleted_user": self.KEPT}
        failures = messy.compare(bad, self.ENTRY, oracle, "10.11")
        self.assertEqual(len(failures), 1)
        self.assertIn("deleted_user", failures[0])
        self.assertIn("Jellyfin 12.2", failures[0])

    def test_a_written_divergence_replaces_the_oracle_for_that_shape_and_lineage_only(self):
        oracle = self.oracle(deleted_user=self.booted("deleted_user", self.KEPT),
                             duplicate_paths=self.booted("duplicate_paths", {"original": True, "duplicate": True}))
        ferrofin = {"deleted_user": self.GONE, "duplicate_paths": {"original": True, "duplicate": True}}
        with mock.patch.dict(messy.DIVERGENCES, {("deleted_user", "10.11"): (self.GONE, "0044 deletes it")}):
            self.assertEqual(messy.compare(ferrofin, self.ENTRY, oracle, "10.11"), [])
            # the other lineage has no divergence written: Jellyfin 12.2's kept rows are the expectation
            self.assertTrue(messy.compare(ferrofin, self.ENTRY, oracle, "12.x"))
            # a divergence names the state Ferrofin must reach: anything else still fails
            changed = {**ferrofin, "deleted_user": {**self.GONE, "Devices": 1}}
            failures = messy.compare(changed, self.ENTRY, oracle, "10.11")
            self.assertEqual(len(failures), 1)
            self.assertIn("written divergence: 0044 deletes it", failures[0])

    def test_a_shape_jellyfin_refuses_needs_a_written_expectation(self):
        oracle = self.oracle(deleted_user={"outcome": "refused", "reason": "FOREIGN KEY constraint failed", "state": {}},
                             duplicate_paths=self.booted("duplicate_paths", {"original": True, "duplicate": False}))
        ferrofin = {"deleted_user": self.GONE, "duplicate_paths": {"original": True, "duplicate": False}}
        failures = messy.compare(ferrofin, self.ENTRY, oracle, "10.11")
        self.assertEqual(len(failures), 1)
        self.assertIn("refused this shape (FOREIGN KEY constraint failed)", failures[0])
        with mock.patch.dict(messy.DIVERGENCES, {("deleted_user", "10.11"): (self.GONE, "Ferrofin boots it")}):
            self.assertEqual(messy.compare(ferrofin, self.ENTRY, oracle, "10.11"), [])

    def test_check_reads_the_fixtures_manifest_and_oracle_record(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            db = fixture / "jellyfin.db"
            with closing(sqlite3.connect(db)) as conn:
                conn.executescript(SCHEMA)
            entry = {"lineage": "test", "shapes": {"duplicate_paths": {"path": "/p", "original": "a" * 32, "duplicate": "b" * 32}}}
            (fixture / "messy.json").write_text(json.dumps(entry))
            (fixture / "messy-oracle.json").write_text(json.dumps(self.oracle(
                duplicate_paths=self.booted("duplicate_paths", {"original": False, "duplicate": False}))))
            with mock.patch.object(messy, "SHAPES", ("duplicate_paths",)):
                self.assertEqual(messy.check(db, fixture), [])
                (fixture / "messy-oracle.json").write_text(json.dumps(self.oracle(
                    duplicate_paths=self.booted("duplicate_paths", {"original": True, "duplicate": False}))))
                self.assertEqual(len(messy.check(db, fixture)), 1)

    def test_shape_results_give_each_shapes_end_state_expectation_source_and_verdict(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            db = fixture / "jellyfin.db"
            with closing(sqlite3.connect(db)) as conn:
                conn.executescript(SCHEMA)
            entry = {"lineage": "test", "shapes": {"duplicate_paths": {"path": "/p", "original": "a" * 32, "duplicate": "b" * 32}}}
            (fixture / "messy.json").write_text(json.dumps(entry))
            for kept, verdict in ((False, "PASS"), (True, "FAIL")):
                (fixture / "messy-oracle.json").write_text(json.dumps(self.oracle(
                    duplicate_paths=self.booted("duplicate_paths", {"original": kept, "duplicate": False}))))
                with mock.patch.object(messy, "SHAPES", ("duplicate_paths",)):
                    results, failures = messy.shape_results(db, fixture)
                self.assertEqual(results, [{"shape": "duplicate_paths", "got": {"original": False, "duplicate": False},
                                            "want": {"original": kept, "duplicate": False}, "source": "Jellyfin 12.2", "verdict": verdict}])
                self.assertEqual(len(failures), 0 if verdict == "PASS" else 1)   # the same judgement as check
                with mock.patch.object(messy, "SHAPES", ("duplicate_paths",)):
                    self.assertEqual(messy.check(db, fixture), failures)

    def test_a_shape_jellyfin_refused_is_a_failed_shape_in_the_results_too(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            db = fixture / "jellyfin.db"
            with closing(sqlite3.connect(db)) as conn:
                conn.executescript(SCHEMA)
            entry = {"lineage": "test", "shapes": {"duplicate_paths": {"path": "/p", "original": "a" * 32, "duplicate": "b" * 32}}}
            (fixture / "messy.json").write_text(json.dumps(entry))
            (fixture / "messy-oracle.json").write_text(json.dumps(self.oracle(
                duplicate_paths={"outcome": "refused", "reason": "constraint", "state": {}})))
            with mock.patch.object(messy, "SHAPES", ("duplicate_paths",)):
                (result,), _ = messy.shape_results(db, fixture)
            self.assertEqual((result["verdict"], result["want"]), ("FAIL", None))
            self.assertIn("refused", result["source"])

    def test_the_shapes_command_prints_the_results_as_json(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            db = fixture / "jellyfin.db"
            with closing(sqlite3.connect(db)) as conn:
                conn.executescript(SCHEMA)
            entry = {"lineage": "test", "shapes": {"duplicate_paths": {"path": "/p", "original": "a" * 32, "duplicate": "b" * 32}}}
            (fixture / "messy.json").write_text(json.dumps(entry))
            (fixture / "messy-oracle.json").write_text(json.dumps(self.oracle(
                duplicate_paths=self.booted("duplicate_paths", {"original": False, "duplicate": False}))))
            out = io.StringIO()
            with mock.patch.object(messy, "SHAPES", ("duplicate_paths",)), contextlib.redirect_stdout(out):
                self.assertEqual(messy.main(["shapes", str(db), str(fixture)]), 0)
            self.assertEqual(json.loads(out.getvalue())[0]["verdict"], "PASS")

    def test_a_fixture_built_with_other_shapes_is_refused_rather_than_partly_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            (fixture / "messy.json").write_text(json.dumps({"lineage": "test", "shapes": {"deleted_user": {}}}))
            (fixture / "messy-oracle.json").write_text("{}")
            with self.assertRaisesRegex(CheckError, "built with shapes deleted_user, not deleted_user, dangling_owner_extra"):
                messy.check(fixture / "jellyfin.db", fixture)


class OracleRunner(unittest.TestCase):
    """run_oracle_server and refusal_reason with docker and the Jellyfin API replaced."""

    def fake_docker(self, running, log=""):
        calls = []

        def run(command, **kwargs):
            calls.append(command)
            out = {"inspect": f"{running}\n", "logs": log}.get(command[1], "")
            return subprocess.CompletedProcess(command, 0, out, "")
        return run, calls

    def server(self, running, api=None, log="", **patches):
        run, calls = self.fake_docker(running, log)
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        config = Path(directory.name) / "config"
        config.mkdir()
        with contextlib.ExitStack() as stack:
            stack.enter_context(mock.patch.object(messy.subprocess, "run", run))
            stack.enter_context(mock.patch.object(messy, "Api", return_value=api or mock.MagicMock()))
            stack.enter_context(mock.patch.object(messy.time, "sleep"))
            for name, value in patches.items():
                stack.enter_context(mock.patch.object(messy, name, value))
            return messy.run_oracle_server(config, config, 18130), calls

    def test_a_running_container_that_answers_booted(self):
        result, calls = self.server("true")
        self.assertEqual(result, {"outcome": "booted", "reason": ""})
        self.assertEqual([call[1] for call in calls if call[0] == "docker"][-3:], ["stop", "logs", "rm"])  # always stopped, logged and removed

    def test_a_container_that_exited_is_a_refusal_naming_its_last_foreign_key_line(self):
        log = "[INF] starting\nfail: warning only\n[ERR] FOREIGN KEY constraint failed: first\n[ERR] FOREIGN KEY constraint failed: last\n"
        result, _ = self.server("false", log=log)
        self.assertEqual(result["outcome"], "refused")
        self.assertEqual(result["reason"], "[ERR] FOREIGN KEY constraint failed: last")

    def test_a_refusal_with_no_known_marker_says_so(self):
        result, _ = self.server("false", log="nothing useful\n")
        self.assertEqual(result, {"outcome": "refused", "reason": "no error line in the log"})

    def test_a_reason_is_cut_to_the_documented_length(self):
        result, _ = self.server("false", log="[ERR] " + "x" * 500)
        self.assertEqual(len(result["reason"]), messy.REASON_CHARS)

    def test_a_container_that_is_slow_but_running_is_an_error_not_a_refusal(self):
        api = mock.MagicMock()
        api.get.side_effect = OSError("not yet")
        with self.assertRaisesRegex(CheckError, "did not answer within 0 s and did not exit"):
            self.server("true", api=api, ORACLE_READY_S=0)


class RecordAndBuild(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        (self.root / "media").mkdir()
        # the files the scratch database's items point at: the templates the messy shapes link
        for template in ("movies/a/a.mkv", "movies/p/p.mkv", "movies/a/extras/x.mkv", "shows/Show/Season 01/e1.mkv"):
            (self.root / "media" / template).parent.mkdir(parents=True, exist_ok=True)
            (self.root / "media" / template).write_bytes(b"synthetic media " + template.encode())
        for name in ("jellyfin-10.11.11", "jellyfin-12.2-from-121"):
            source = self.root / name
            (source / "data").mkdir(parents=True)
            populate(source / "data/jellyfin.db")
            (source / "synthetic-ready.json").write_text("{}")
            (source / "synthetic.json").write_text(json.dumps(MANIFEST))
        self.boots = []
        self.media = []

    def oracle(self, command, **kwargs):
        if command[:2] == ["docker", "pull"] or command[:3] == ["docker", "image", "inspect"]:
            return subprocess.CompletedProcess(command, 0, "sha256:image\n", "")
        raise AssertionError(command)

    def booted(self, outcome="booted", combined=None):
        def run(config, media, port):
            self.boots.append(Path(config).name)
            self.media.append(Path(media))
            if combined and Path(config).name.endswith("-all"):
                return dict(combined)
            return {"outcome": outcome, "reason": ""}
        return run

    def record(self, **kwargs):
        with mock.patch.object(messy.subprocess, "run", self.oracle), mock.patch.object(messy, "run_oracle_server", self.booted(**kwargs)):
            return messy.record_oracle(self.root, self.root / "jellyfin-10.11.11", messy.SHAPES, MANIFEST, 18130)

    def test_each_shape_gets_a_clean_copy_of_its_own_and_one_more_carries_them_all(self):
        record = self.record()
        self.assertEqual([name.split("jellyfin-10.11.11-")[1] for name in self.boots], [*messy.SHAPES, "all"])
        self.assertEqual(set(record["shapes"]), set(messy.SHAPES))
        self.assertEqual(record["image"], messy.ORACLE_IMAGE)
        self.assertEqual(record["image_id"], "sha256:image")
        # the single-shape copy carries only its shape, and the scratch is removed afterwards
        self.assertEqual(set(record["shapes"]["deleted_user"]["state"]), {"deleted_user"})
        self.assertEqual(list((self.root / "work").iterdir()), [])

    def test_a_combined_run_that_differs_from_the_single_runs_is_an_error_naming_both_states(self):
        with self.assertRaisesRegex(CheckError, "handled .* differently with all the shapes present"):
            self.record(combined={"outcome": "refused", "reason": "x"})
        record = {"outcome": "booted", "reason": "", "state": {shape: {"alone": shape} for shape in messy.SHAPES}}
        record["state"]["orphan_ancestor"] = {"with": "all"}
        with self.assertRaisesRegex(CheckError, r"orphan_ancestor: alone \{'alone': 'orphan_ancestor'\}, with all \{'with': 'all'\}"):
            with mock.patch.object(messy.subprocess, "run", self.oracle), \
                 mock.patch.object(messy, "run_oracle_server", lambda config, media, port: dict(record)), \
                 mock.patch.object(messy, "read_state", lambda db, entry: {s: ({"with": "all"} if Path(db).parent.parent.name.endswith("-all") and s == "orphan_ancestor"
                                                                              else {"alone": s}) for s in entry["shapes"]}):
                messy.record_oracle(self.root, self.root / "jellyfin-10.11.11", messy.SHAPES, MANIFEST, 18130)

    def test_every_oracle_boot_mounts_the_messy_media_tree_and_the_build_links_the_files_into_it(self):
        self.record()
        self.assertEqual(set(self.media), {self.root / "media-messy"})
        record = {"shapes": {shape: {"outcome": "booted"} for shape in messy.SHAPES}}
        with mock.patch.object(messy, "record_oracle", return_value=record):
            messy.build(self.root, 18130)
        linked = self.root / "media-messy/movies/Synthetic dangling extras/extra.mkv"
        self.assertTrue(linked.samefile(self.root / "media/movies/a/extras/x.mkv"))
        self.assertFalse((self.root / "media/movies/Synthetic dangling extras").exists())   # the shared tree is untouched

    def test_a_failed_boot_leaves_no_scratch_copy(self):
        def boom(config, media, port):
            raise CheckError("docker went away")
        with mock.patch.object(messy.subprocess, "run", self.oracle), mock.patch.object(messy, "run_oracle_server", boom):
            with self.assertRaises(CheckError):
                messy.record_oracle(self.root, self.root / "jellyfin-10.11.11", messy.SHAPES, MANIFEST, 18130)
        self.assertEqual(list((self.root / "work").iterdir()), [])

    def test_a_failed_build_removes_its_derived_copy_and_leaves_the_clean_fixtures(self):
        with mock.patch.object(messy, "record_oracle", side_effect=CheckError("no oracle")):
            with self.assertRaisesRegex(CheckError, "no oracle"):
                messy.build(self.root, 18130)
        self.assertFalse((self.root / "jellyfin-10.11.11-messy").exists())
        self.assertTrue((self.root / "jellyfin-10.11.11/data/jellyfin.db").exists())

    def test_a_finished_build_is_kept_and_a_second_build_does_nothing(self):
        record = {"shapes": {shape: {"outcome": "booted"} for shape in messy.SHAPES}}
        with mock.patch.object(messy, "record_oracle", return_value=record) as recorded:
            messy.build(self.root, 18130)
            self.assertEqual(recorded.call_count, 2)
            messy.build(self.root, 18130)
            self.assertEqual(recorded.call_count, 2)
        derived = json.loads((self.root / "jellyfin-12.2-messy/messy.json").read_text())
        self.assertEqual((derived["lineage"], derived["source"], set(derived["shapes"])),
                         ("12.x", "jellyfin-12.2-from-121", set(messy.SHAPES)))

    def test_a_built_fixture_with_other_shapes_is_refused_not_reused(self):
        record = {"shapes": {shape: {"outcome": "booted"} for shape in messy.SHAPES}}
        with mock.patch.object(messy, "record_oracle", return_value=record):
            messy.build(self.root, 18130)
        with mock.patch.object(messy, "SHAPES", ("deleted_user",)):
            with self.assertRaisesRegex(CheckError, "built with shapes .*build into a new fixtures directory"):
                messy.build(self.root, 18130)

    def test_a_missing_source_fixture_is_named(self):
        (self.root / "jellyfin-10.11.11/synthetic-ready.json").unlink()
        with self.assertRaisesRegex(CheckError, "jellyfin-10.11.11 is not built"):
            messy.build(self.root, 18130)


class MediaTree(unittest.TestCase):
    """The messy rows mount a media tree of their own: the shared one plus a link per seeded file."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        (self.root / "media/movies/a").mkdir(parents=True)
        (self.root / "media/movies/a/a.mkv").write_bytes(b"movie")
        self.entry = {"shapes": {"x": {"files": [["/media/movies/Synthetic x/y/new.mkv", "/media/movies/a/a.mkv"]]}, "y": {}}}

    def test_a_seeded_file_is_a_hard_link_to_its_template_in_a_tree_of_its_own(self):
        messy.link_media(self.root, self.entry)
        linked = self.root / "media-messy/movies/Synthetic x/y/new.mkv"
        self.assertTrue(linked.samefile(self.root / "media/movies/a/a.mkv"))
        self.assertTrue((self.root / "media-messy/movies/a/a.mkv").samefile(self.root / "media/movies/a/a.mkv"))
        # the shared tree, which the clean rows scan, gains nothing
        self.assertEqual(sorted(path.name for path in (self.root / "media").rglob("*")), ["a", "a.mkv", "movies"])

    def test_linking_twice_changes_nothing_and_a_missing_template_is_named(self):
        messy.link_media(self.root, self.entry)
        messy.link_media(self.root, self.entry)
        self.entry["shapes"]["y"] = {"files": [["/media/movies/b/new.mkv", "/media/movies/nope.mkv"]]}
        with self.assertRaisesRegex(CheckError, "the template file /media/movies/nope.mkv is not in"):
            messy.link_media(self.root, self.entry)

    def test_every_shape_that_creates_an_item_lists_the_file_it_needs_and_has_its_row_there(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        db = str(Path(directory.name) / "jellyfin.db")
        populate(db)
        entry = messy.seed_messy(db, MANIFEST)
        with closing(sqlite3.connect(db)) as conn:
            for shape in ("dangling_owner_extra", "guid_casing", "virtual_season", "non_ascii_names"):
                with self.subTest(shape):
                    files = entry["shapes"][shape]["files"]
                    self.assertTrue(files)
                    for path, template in files:
                        self.assertTrue(path.startswith("/media/") and template.startswith("/media/"))
                        self.assertEqual(conn.execute("SELECT COUNT(*) FROM BaseItems WHERE Path = ?", (path,)).fetchone()[0], 1, path)

    def test_a_half_copied_tree_is_never_taken_for_a_whole_one(self):
        (self.root / "media-messy.partial").mkdir()
        (self.root / "media-messy.partial/leftover").write_bytes(b"x")
        messy.link_media(self.root, self.entry)
        self.assertFalse((self.root / "media-messy.partial").exists())
        self.assertFalse((self.root / "media-messy/leftover").exists())
        self.assertTrue((self.root / "media-messy/movies/a/a.mkv").exists())

    def test_a_path_outside_media_is_an_error_not_a_link_outside_the_tree(self):
        self.entry["shapes"]["y"] = {"files": [["/etc/escape.mkv", "/media/movies/a/a.mkv"]]}
        with self.assertRaisesRegex(CheckError, "must be container paths under /media/"):
            messy.link_media(self.root, self.entry)

    def test_a_shape_whose_file_is_meant_to_be_gone_lists_none(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        db = str(Path(directory.name) / "jellyfin.db")
        populate(db)
        entry = messy.seed_messy(db, MANIFEST, ("gone_file_userdata",))
        self.assertNotIn("files", entry["shapes"]["gone_file_userdata"])


class WrittenDivergences(unittest.TestCase):
    KEYS = {"deleted_user": set(messy.USER_TABLES), "dangling_owner_extra": {"exists", "owner"},
            "duplicate_paths": {"original", "duplicate"},
            "guid_casing": {"owner_lower36", "owner_n32", "primary_lower36", "primary_n32"},
            "case_duplicates": {"person_item_ascii", "people_ascii", "unicode", "artists"},
            "orphan_ancestor": {"missing_parent_rows", "missing_item_rows"},
            "virtual_season": {"season", "episode"},
            "gone_file_userdata": {"item", "userdata_on_item", "userdata_with_key"},
            "non_ascii_names": {"users", "people", "path"}}

    def test_each_divergence_names_a_seeded_shape_a_derived_lineage_and_a_whole_state_with_a_reason(self):
        lineages = {lineage for _, _, lineage in messy.DERIVED}
        self.assertTrue(messy.DIVERGENCES)
        for (shape, lineage), (state, why) in messy.DIVERGENCES.items():
            with self.subTest((shape, lineage)):
                self.assertIn(shape, messy.SHAPES)
                self.assertIn(lineage, lineages)
                self.assertEqual(set(state), self.KEYS[shape])
                self.assertGreater(len(why), 20)


class Helpers(unittest.TestCase):
    def test_spelled_follows_the_stored_id(self):
        value = uuid.UUID("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9")
        self.assertEqual(messy.spelled("0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9", value), str(value).upper())
        self.assertEqual(messy.spelled("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9", value), str(value))
        self.assertEqual(messy.spelled("0A1B2C3D4E5F60718293A4B5C6D7E8F9", value), value.hex.upper())
        self.assertEqual(messy.spelled("0a1b2c3d4e5f60718293a4b5c6d7e8f9", value), value.hex)

    def test_each_derived_fixture_copies_a_fixture_the_builder_makes_and_a_run_row_adopts(self):
        from synthetic import VERSIONS
        built = {name for name, _, _ in VERSIONS}
        run_sh = (Path(__file__).parents[1] / "run.sh").read_text()
        for name, source, _ in messy.DERIVED:
            with self.subTest(name):
                self.assertIn(source, built)
                self.assertIn(f"|{name}|", run_sh)   # the MESSY_TABLE row that adopts it


if __name__ == "__main__":
    unittest.main()
