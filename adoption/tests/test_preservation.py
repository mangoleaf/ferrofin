"""Regression tests for synthetic coverage and the unchanged-scan write audit."""

from contextlib import closing
import copy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import posixpath
import shutil
import sqlite3
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
import uuid

sys.path.insert(0, str(Path(__file__).parents[1]))
from metadata import CheckError
from preservation import (FROM_ORACLE, GENERATIONS, ORACLE, comparable, compare, expectation, normalize, run_matrix,
                          seed_failures, seed_state, validate, validate_oracle, validate_seed_state, same_artwork,
                          verify_database)
from synthetic import VERSIONS
import synthetic_fixture
from synthetic_fixture import USERS, drop_dangling_chain, seed_database, wait_for_live_members
from watch_history import orphaned_items
from unchanged_scan import TABLES, audit, assert_unchanged, check
from test_user_accounts import fixture as account_fixture


class WriteAuditTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.db = Path(tmp.name) / 'test.db'
        with closing(sqlite3.connect(self.db)) as conn, conn:
            for table in TABLES:
                conn.execute(f'CREATE TABLE "{table}" (Id INTEGER, Type TEXT)')
                conn.execute(f'INSERT INTO "{table}" VALUES (1, "Movie")')

    def sql(self, statement):
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.executescript(statement)

    def test_read_only_scan_passes(self):
        with audit(self.db) as conn:
            self.sql('SELECT * FROM BaseItems;')
            assert_unchanged(conn)

    def test_same_value_update_is_detected(self):
        with audit(self.db) as conn:
            self.sql('UPDATE BaseItems SET Type=Type;')
            with self.assertRaisesRegex(CheckError, 'BaseItems UPDATE=1'):
                assert_unchanged(conn)

    def test_identical_image_delete_and_reinsert_is_detected(self):
        with audit(self.db) as conn:
            self.sql('DELETE FROM BaseItemImageInfos; INSERT INTO BaseItemImageInfos VALUES (1,"Movie");')
            with self.assertRaisesRegex(CheckError, 'BaseItemImageInfos DELETE=1'):
                assert_unchanged(conn)

    def test_every_related_table_is_audited(self):
        for table in TABLES:
            with self.subTest(table=table), audit(self.db) as conn:
                self.sql(f'UPDATE "{table}" SET Type=Type;')
                with self.assertRaises(CheckError):
                    assert_unchanged(conn)

    def test_folder_bookkeeping_is_allowed(self):
        self.sql('UPDATE BaseItems SET Type="MediaBrowser.Controller.Entities.Folder";')
        with audit(self.db) as conn:
            self.sql('UPDATE BaseItems SET Id=Id;')
            assert_unchanged(conn)

    def test_audit_is_removed_after_failure(self):
        with self.assertRaisesRegex(RuntimeError, 'scan failed'):
            with audit(self.db):
                raise RuntimeError('scan failed')
        with closing(sqlite3.connect(self.db)) as conn:
            self.assertEqual(conn.execute("SELECT name FROM sqlite_master WHERE name LIKE 'AdoptionTest%'").fetchall(), [])

    def test_subtitle_request_is_detected_even_without_database_writes(self):
        class Api:
            scans = 0
            def call(self, *args, **kwargs):
                return f'ferrofin_metadata_provider_requests_total{{provider="opensubtitles",result="success"}} {self.scans}\n'.encode()
            def scan(self, *args, **kwargs):
                self.scans += 1
        with self.assertRaisesRegex(CheckError, 'provider requests'):
            check(Api(), self.db)

    def test_missing_metric_endpoint_fails(self):
        class Api:
            def call(self, *args, **kwargs):
                raise CheckError('metrics unavailable')
        with self.assertRaisesRegex(CheckError, 'metrics unavailable'):
            check(Api(), self.db)

    def test_new_subtitle_file_is_detected(self):
        class Api:
            def call(self, *args, **kwargs):
                return b'ferrofin_metadata_provider_requests_total{provider="opensubtitles",result="success"} 0\n'
            def scan(inner, *args, **kwargs):
                self.db.with_suffix('.srt').write_text('duplicate subtitle')
        with self.assertRaisesRegex(CheckError, 'duplicated subtitle files'):
            check(Api(), self.db, (self.db.parent,))

    def test_changed_subtitle_contents_are_detected(self):
        path = self.db.with_suffix('.srt')
        path.write_text('original')
        class Api:
            def call(self, *args, **kwargs):
                return b''
            def scan(self, *args, **kwargs):
                path.write_text('changed')
        with self.assertRaisesRegex(CheckError, 'changed or duplicated subtitle files'):
            check(Api(), self.db, (self.db.parent,))

    def test_broken_foreign_key_fails_integrity_gate(self):
        self.sql('CREATE TABLE Children(ParentId INTEGER REFERENCES BaseItems(rowid));')
        # Use a valid unique parent key and an intentionally dangling reference.
        self.sql('DROP TABLE Children; CREATE TABLE Parents(Id INTEGER PRIMARY KEY); '
                 'CREATE TABLE Children(ParentId INTEGER REFERENCES Parents(Id)); '
                 'INSERT INTO Children VALUES (99);')
        with self.assertRaisesRegex(CheckError, 'foreign-key'):
            verify_database(self.db)


class FixtureValidationTest(unittest.TestCase):
    def setUp(self):
        accounts_manifest, accounts = account_fixture()
        self.manifest = {**accounts_manifest, 'allowed_movie': 'allowed', 'rated_movie': 'rated', 'private_movie': 'private'}
        self.data = {
            'accounts': accounts,
            'visibility': {USERS[1]: ['allowed', 'rated', 'private'], USERS[2]: ['allowed']},
            'manual': {'LockData': True, 'LockedFields': ['Name']},
            'custom_artwork': {'size': [100, 150], 'rgb': [0, 255, 255]},
            'subtitles': {'movie': [('eng', True, 'subrip')]},
            'versions': ['one', 'two'], 'extras': ['extra'], 'collection': ['first', 'second'],
            'playlists': {'playlist': {'items': ['third', 'first', 'second'], 'shares': ['child']}},
            'relationships': {str(i): {'Type': kind, 'ParentIndexNumber': 2} for i, kind in enumerate(
                ['Audio'] * 6 + ['MusicAlbum'] * 2 + ['MusicArtist'] * 2)},
            'views': {name: {'resume': ['movie'], 'next_up': ['episode']} for name in USERS[1:]},
        }

    def test_complete_synthetic_fixture_passes(self):
        validate(self.data, self.manifest)

    def test_missing_restriction_cannot_silently_skip(self):
        self.data['visibility'][USERS[2]].append('rated')
        with self.assertRaisesRegex(CheckError, 'parental rating restriction'):
            validate(self.data, self.manifest)

    def test_empty_music_fixture_cannot_pass(self):
        self.data['relationships'] = {}
        with self.assertRaisesRegex(CheckError, 'music tracks'):
            validate(self.data, self.manifest)

    def test_empty_derived_view_cannot_pass(self):
        self.data['views'][USERS[2]]['next_up'] = []
        with self.assertRaisesRegex(CheckError, 'next up views'):
            validate(self.data, self.manifest)

    def test_count_preserving_playlist_reorder_is_detected(self):
        actual = copy.deepcopy(self.data)
        actual['playlists']['playlist']['items'].reverse()
        self.assertEqual(compare(self.data, actual), ['playlists'])

    def test_same_count_different_membership_is_detected(self):
        actual = copy.deepcopy(self.data)
        actual['collection'][0] = 'different'
        self.assertEqual(compare(self.data, actual), ['collection'])

    def test_missing_subtitle_cannot_silently_skip(self):
        self.data['subtitles']['movie'] = []
        with self.assertRaisesRegex(CheckError, 'English subtitles'):
            validate(self.data, self.manifest)

    def test_local_poster_cannot_stand_in_for_uploaded_artwork(self):
        self.data['custom_artwork']['rgb'] = [255, 165, 0]
        with self.assertRaisesRegex(CheckError, 'uploaded artwork'):
            validate(self.data, self.manifest)

    def test_small_decoder_rounding_is_allowed_but_wrong_color_fails(self):
        expected = {'size': [100, 150], 'rgb': [0, 255, 255]}
        self.assertTrue(same_artwork(expected, {'size': [100, 150], 'rgb': [1, 254, 254]}))
        self.assertFalse(same_artwork(expected, {'size': [100, 150], 'rgb': [255, 165, 0]}))
        self.assertFalse(same_artwork(expected, {'size': [100, 100], 'rgb': [0, 255, 255]}))

    def test_empty_optional_similarity_settings_are_equivalent(self):
        before = {'libraries': {'library': {'TypeOptions': [{
            'Type': 'Movie', 'SimilarItemProviders': [], 'SimilarItemProviderOrder': [],
            'MetadataFetchers': []}]}}}
        after = {'libraries': {'library': {'TypeOptions': [{'Type': 'Movie', 'MetadataFetchers': []}]}}}
        self.assertFalse(compare(before, after))
        after['libraries']['library']['TypeOptions'][0].pop('MetadataFetchers')
        self.assertEqual(compare(before, after), ['libraries'])

    def test_nonempty_provider_order_is_compared_exactly(self):
        before = {'libraries': {'library': {'TypeOptions': [{
            'SimilarItemProviderOrder': ['first', 'second']}]}}}
        after = copy.deepcopy(before)
        after['libraries']['library']['TypeOptions'][0]['SimilarItemProviderOrder'].reverse()
        self.assertEqual(compare(before, after), ['libraries'])

    def test_guid_representation_is_normalized(self):
        self.assertEqual(normalize({'id': 'AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE'}),
                         {'id': 'aaaaaaaabbbbccccddddeeeeeeeeeeee'})


def guid(number):
    return str(uuid.UUID(int=number)).upper()


MOVIE = 'MediaBrowser.Controller.Entities.Movies.Movie'
EPISODE = 'MediaBrowser.Controller.Entities.TV.Episode'
MOVIES = ('allowed_movie', 'rated_movie', 'locked_movie', 'versions_movie', 'private_movie')
AUDIO = 'MediaBrowser.Controller.Entities.Audio.Audio'
BOXSET = 'MediaBrowser.Controller.Entities.Movies.BoxSet'
PLAYLIST = 'MediaBrowser.Controller.Playlists.Playlist'
TRACKS = {0xa1: ('track 1', '/media/music/Artist/Album/1-01 Track.flac'),
          0xa2: ('track 2', '/media/music/Artist/Album/1-02 Track.flac')}
ADULT, CHILD = guid(0xad), guid(0xc1)
# What Jellyfin 12.2 scores the seeded ratings (the oracle's InheritedParentalRating*).
SCORES = {'SAM 13': (13, None), 'FSK 16': (16, None), '-12': (12, None), 'MA15+': (15, 3), '12A': (12, 0)}


def movie_path(key):
    return f'/media/movies/{key}/{key}.mkv'


def canned_10_11_8(path):
    """A stopped 10.11.8 fixture database with only the tables and rows the seeds touch."""
    manifest = {'users': {USERS[1]: uuid.UUID(ADULT).hex, USERS[2]: uuid.UUID(CHILD).hex}}
    with closing(sqlite3.connect(path)) as conn, conn:
        conn.executescript(f'''
            CREATE TABLE BaseItems (Id TEXT PRIMARY KEY, Type TEXT, Name TEXT, Path TEXT, Data TEXT,
                ParentId TEXT REFERENCES BaseItems (Id),
                OwnerId TEXT, TopParentId TEXT, SeriesId TEXT, SeasonId TEXT, PresentationUniqueKey TEXT,
                OfficialRating TEXT, InheritedParentalRatingValue INTEGER, InheritedParentalRatingSubValue INTEGER);
            CREATE TABLE BaseItemProviders (ItemId TEXT, ProviderId TEXT, ProviderValue TEXT);
            CREATE TABLE Users (Id TEXT PRIMARY KEY, Username TEXT);
            CREATE TABLE UserData (ItemId TEXT, UserId TEXT, CustomDataKey TEXT, AudioStreamIndex INTEGER,
                IsFavorite INTEGER NOT NULL, LastPlayedDate TEXT, Likes INTEGER, PlayCount INTEGER NOT NULL,
                PlaybackPositionTicks INTEGER NOT NULL, Played INTEGER NOT NULL, Rating REAL,
                SubtitleStreamIndex INTEGER, RetentionDate TEXT, PRIMARY KEY (ItemId, UserId, CustomDataKey));
            INSERT INTO Users VALUES ('{ADULT}', '{USERS[1]}'), ('{CHILD}', '{USERS[2]}');
            INSERT INTO BaseItems (Id, Type, Name, ParentId, TopParentId, SeriesId, SeasonId, OfficialRating)
              VALUES ('{guid(0x5e)}', 'MediaBrowser.Controller.Entities.TV.Season', 'Season 1', NULL, '{guid(0x70)}', NULL, NULL, 'G'),
                     ('{guid(0xe1)}', '{EPISODE}', 'Episode 1', '{guid(0x5e)}', '{guid(0x70)}', '{guid(0x5)}', '{guid(0x5e)}', 'G');
        ''')
        for number, key in enumerate(MOVIES, 1):
            manifest[key] = uuid.UUID(guid(number)).hex
            conn.execute("INSERT INTO BaseItems (Id, Type, Name, Path, OfficialRating, InheritedParentalRatingValue) "
                         "VALUES (?, ?, ?, ?, 'G', 0)", (guid(number), MOVIE, key, movie_path(key)))
        conn.executemany('INSERT INTO BaseItems (Id, Type, Name, Path) VALUES (?, ?, ?, ?)',
                         [(guid(number), AUDIO, name, path) for number, (name, path) in TRACKS.items()])
        # The linked containers as 10.11.8 wrote them: one live member each.
        for number, kind, key, path, member, member_path in (
                (0xb0, BOXSET, 'linked_collection', '%AppDataPath%/collections/Synthetic linked collection [boxset]',
                 guid(MOVIES.index('rated_movie') + 1), movie_path('rated_movie')),
                (0xb1, PLAYLIST, 'linked_playlist', '%AppDataPath%/playlists/Synthetic linked playlist',
                 guid(0xa2), TRACKS[0xa2][1]),
                (0xb2, PLAYLIST, 'linked_pathless', '%AppDataPath%/playlists/Synthetic path-less playlist',
                 guid(0xa1), TRACKS[0xa1][1])):
            manifest[key] = uuid.UUID(guid(number)).hex
            data = {'LinkedChildren': [{'Path': member_path, 'Type': 'Manual', 'ItemId': uuid.UUID(member).hex}]}
            conn.execute('INSERT INTO BaseItems (Id, Type, Name, Path, Data) VALUES (?, ?, ?, ?, ?)',
                         (guid(number), kind, key, path, json.dumps(data)))
        locked = guid(MOVIES.index('locked_movie') + 1)
        conn.executemany('INSERT INTO BaseItemProviders VALUES (?, ?, ?)',
                         [(locked, 'Tmdb', '999999991'), (locked, 'Imdb', 'tt99999991')])
        for key in (str(uuid.UUID(locked)).lower(), '999999991', 'tt99999991'):
            conn.execute("INSERT INTO UserData (ItemId, UserId, CustomDataKey, IsFavorite, LastPlayedDate, PlayCount, "
                         "PlaybackPositionTicks, Played) VALUES (?, ?, ?, 0, '2026-01-06 12:00:00', 0, 2000000000, 0)",
                         (locked, ADULT, key))
    return manifest


def jellyfin_12_2(path, seeds):
    """What Jellyfin 12.2 (and, by contract, Ferrofin) makes of the seeds: scored ratings,
    harmonized rows, the orphaned chain gone with its history on the placeholder, and the
    linked containers' entries migrated into LinkedChildren rows."""
    with closing(sqlite3.connect(path)) as conn, conn:
        for movie, rating in seeds['ratings'].items():
            conn.execute('UPDATE BaseItems SET InheritedParentalRatingValue = ?, InheritedParentalRatingSubValue = ? '
                         "WHERE lower(replace(Id, '-', '')) = ?", (*SCORES[rating], movie))
        winner, loser = seeds['conflict']['keys']
        conn.execute('UPDATE UserData SET (IsFavorite, LastPlayedDate, PlayCount, PlaybackPositionTicks, Played) = '
                     '(SELECT IsFavorite, LastPlayedDate, PlayCount, PlaybackPositionTicks, Played FROM UserData '
                     'WHERE CustomDataKey = ?) WHERE CustomDataKey = ?', (winner, loser))
        orphans = [str(uuid.UUID(item)).upper() for item in seeds['orphans']]
        conn.execute("UPDATE UserData SET ItemId = '00000000-0000-0000-0000-000000000001', "
                     "RetentionDate = '2026-10-12 08:00:00.0000000' WHERE ItemId IN (?, ?)", orphans)
        conn.execute('DELETE FROM BaseItems WHERE Id IN (?, ?)', orphans)
        conn.execute('CREATE TABLE LinkedChildren (ParentId TEXT, ChildId TEXT, ChildType INTEGER, SortOrder INTEGER)')
        for seed in seeds['linked_children'].values():
            conn.executemany('INSERT INTO LinkedChildren VALUES (?, ?, 0, ?)', [
                (str(uuid.UUID(seed['id'])).upper(), str(uuid.UUID(child)).upper(), order)
                for order, child in enumerate(seed['members'])])


class OracleExpectationTest(unittest.TestCase):
    # The synthetic fixture's "Twin Horizon" group: 10.11 serves its alternate under the
    # Video-derived id, every 12.x under the Movie-derived one.
    PRIMARY, AS_VIDEO, AS_MOVIE = ('711d7b0f6790b40fbca3fd771dbb96d3', 'e24bb924be539d0746ec205b0b9536e6',
                                   'ad1cd20859f6c42cfdfd85714b08748d')

    def setUp(self):
        self.baseline = {'versions': [self.PRIMARY, self.AS_VIDEO], 'extras': ['extra']}
        self.oracle = {'versions': [self.PRIMARY, self.AS_MOVIE], 'extras': ['oracle-only']}

    def test_a_10_11_path_expects_the_version_ids_jellyfin_12_2_serves(self):
        expected = expectation('10.11.9', self.baseline, self.oracle, self.oracle)
        self.assertEqual(compare(expected, {'versions': [self.PRIMARY, self.AS_MOVIE], 'extras': ['extra']}), [])
        # Keeping the 10.11 id is not what 12.2 does.
        self.assertEqual(compare(expected, {'versions': [self.PRIMARY, self.AS_VIDEO], 'extras': ['extra']}),
                         ['versions'])

    def test_only_the_oracle_keys_leave_the_source_baseline(self):
        self.assertEqual(FROM_ORACLE, ('versions',))
        expected = expectation('10.11.8', self.baseline, self.oracle, self.oracle)
        self.assertEqual(expected['extras'], ['extra'])
        self.assertEqual(self.baseline['versions'], [self.PRIMARY, self.AS_VIDEO], 'the baseline is not modified')

    def test_a_12_x_path_keeps_its_own_release_baseline(self):
        own = {'versions': ['own-12.1-ids'], 'extras': ['extra']}
        disagreeing = {'versions': [self.PRIMARY, self.AS_VIDEO]}
        # A 12.x source never consults the 10.11 upgrade, even when it disagrees.
        self.assertIs(expectation('12.1', own, self.oracle, disagreeing), own)

    def test_the_oracle_must_agree_with_the_real_10_11_upgrade(self):
        upgraded = {'versions': [self.PRIMARY, self.AS_VIDEO]}
        with self.assertRaisesRegex(CheckError, 'serve different versions'):
            expectation('10.11.8', self.baseline, self.oracle, upgraded)


class ServerReadyTest(unittest.TestCase):
    def test_ready_waits_out_the_10_11_setup_server(self):
        # 10.11's SetupServer answers /System/Info/Public during migrations and 503s the
        # rest (SetupServer.cs); a seed call sent then is reset at the hand-over (gl-662).
        # The main host then answers /System/Ping before startup completes.
        phase = ['setup']
        answers = {'setup': '/System/Info/Public', 'starting': '/System/Ping'}

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                self.send_response(200 if phase[0] == 'main' or self.path == answers[phase[0]] else 503)
                self.send_header('Content-Type', 'application/json')
                self.end_headers()
                self.wfile.write(b'{}')
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        ready = threading.Thread(target=synthetic_fixture.Api(f'http://127.0.0.1:{server.server_port}').ready,
                                 kwargs={'timeout': 30}, daemon=True)
        ready.start()
        for current in answers:
            phase[0] = current
            ready.join(1.5)
            self.assertTrue(ready.is_alive(), f'ready() returned in the {current} phase')
        phase[0] = 'main'
        ready.join(10)
        self.assertFalse(ready.is_alive())


class SeededAdoptionTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.source = self.dir / 'source.db'
        self.manifest = canned_10_11_8(self.source)
        self.seeds = seed_database(self.source, self.manifest)
        self.manifest['seeds'] = self.seeds
        self.oracle_db, self.adopted = self.dir / 'oracle.db', self.dir / 'adopted.db'
        for target in (self.oracle_db, self.adopted):
            shutil.copyfile(self.source, target)
            jellyfin_12_2(target, self.seeds)
        # The child's limit is PG-13 (13).
        child = sorted(movie for movie, rating in self.seeds['ratings'].items() if SCORES[rating][0] <= 13)
        # The child resumes the allowed movie; harmonized, the adult's conflict is played.
        allowed = self.manifest['allowed_movie']
        linked = self.seeds['linked_children']
        self.oracle = {'visibility': {USERS[1]: sorted(self.seeds['ratings']), USERS[2]: child},
                       'views': {USERS[1]: {'resume': ['episode']}, USERS[2]: {'resume': [allowed]}},
                       'linked_children': {'collection': sorted(linked['collection']['members']),
                                           'playlist': linked['playlist']['members'],
                                           'pathless': linked['pathless']['members']}}
        self.actual = copy.deepcopy(self.oracle)
        # What the 10.11.8 fixture itself still holds: the chain, with its history.
        self.state = seed_state(self.source, self.seeds)

    def sql(self, path, statement, *args):
        with closing(sqlite3.connect(path)) as conn, conn:
            conn.execute(statement, args)

    def failures(self):
        with closing(sqlite3.connect(self.adopted)) as conn, closing(sqlite3.connect(self.oracle_db)) as oracle:
            return seed_failures(conn, self.actual, self.seeds, self.state, oracle, self.oracle)

    def test_seeds_land_in_the_10_11_8_database(self):
        with closing(sqlite3.connect(self.source)) as conn:
            ratings = dict(conn.execute('SELECT Name, OfficialRating FROM BaseItems WHERE Type = ?', (MOVIE,)))
            rows = conn.execute("SELECT CustomDataKey, LastPlayedDate, PlayCount, PlaybackPositionTicks, Played "
                                "FROM UserData WHERE UserId = ? AND ItemId = ? ORDER BY 1",
                                (ADULT, guid(3))).fetchall()
            orphans = orphaned_items(conn)
            chain = conn.execute('SELECT Id, ParentId, Type, SeriesId, TopParentId FROM BaseItems '
                                 "WHERE Name LIKE 'Synthetic orphan%' ORDER BY Name").fetchall()
            history = conn.execute('SELECT ItemId, CustomDataKey, Played, PlayCount FROM UserData '
                                   'WHERE ItemId = ?', (chain[1][0],)).fetchall()
        self.assertEqual(ratings, {'allowed_movie': 'SAM 13', 'rated_movie': 'FSK 16', 'locked_movie': '-12',
                                   'versions_movie': 'MA15+', 'private_movie': '12A'})
        # Exactly upstream's two conflicting rows; the GUID-keyed row that agreed with
        # neither (and would have won on date) is gone.
        self.assertEqual(rows, [('999999991', '2023-08-14 00:00:00', 9, 0, 1),
                                ('tt99999991', '2021-12-31 00:00:00', 7, 490, 0)])
        self.assertEqual(orphans, set(self.seeds['orphans']))
        self.assertEqual([row[2:] for row in chain], [(EPISODE, None, None)] * 2)
        self.assertEqual(chain[1][1], chain[0][0])
        self.assertTrue(chain[0][0].isupper() and '-' in chain[0][0], 'ids are spelled like the source rows')
        self.assertEqual(history, [(chain[1][0], self.seeds['orphan_history']['key'], 1, 3)])

    def test_linked_children_seeds_land_beside_the_live_member(self):
        linked = self.seeds['linked_children']
        rated, allowed, locked, versions = (uuid.UUID(guid(MOVIES.index(key) + 1)).hex for key in (
            'rated_movie', 'allowed_movie', 'locked_movie', 'versions_movie'))
        first, second = (uuid.UUID(guid(number)).hex for number in TRACKS)
        with closing(sqlite3.connect(self.source)) as conn:
            stored = {role: json.loads(conn.execute('SELECT Data FROM BaseItems WHERE Id = ?', (
                str(uuid.UUID(seed['id'])).upper(),)).fetchone()[0])['LinkedChildren'] for role, seed in linked.items()}
            items = {row[0] for row in conn.execute("SELECT lower(replace(Id, '-', '')) FROM BaseItems")}
        self.assertEqual(stored, {role: seed['entries'] for role, seed in linked.items()})
        collection, playlist, pathless = (linked[role]['entries'] for role in ('collection', 'playlist', 'pathless'))
        self.assertEqual(collection[0], {'Path': movie_path('rated_movie'), 'Type': 'Manual', 'ItemId': rated})
        # Only the live members' cached ItemIds have rows; the unindexed path is the member's subtitle.
        cached = [entry['ItemId'] for entry in collection + playlist + pathless if 'ItemId' in entry]
        self.assertEqual([item in items for item in cached], [True, False, False, False, True, False, True, True])
        self.assertEqual(collection[3]['Path'], '/media/movies/rated_movie/rated_movie.eng.srt')
        self.assertEqual(collection[4], {'Path': collection[3]['Path'], 'Type': 'Manual', 'LibraryItemId': locked})
        # The path outranks LibraryItemId, so the versions movie never becomes a member.
        self.assertEqual(collection[5], {'Path': movie_path('allowed_movie'), 'Type': 'Manual', 'LibraryItemId': versions})
        # From the container's folder one relative entry reaches the other track and one no
        # item; from "/" (where 10.11's clean-up looks) both reach it.
        folder = '/config/data/playlists/Synthetic linked playlist'
        for entry, from_folder in ((playlist[2], TRACKS[0xa1][1]), (playlist[3], '/config/media/music/Artist/Album/'
                                                                   '1-01 Track.flac')):
            self.assertFalse(posixpath.isabs(entry['Path']))
            self.assertEqual(posixpath.normpath(posixpath.join(folder, entry['Path'])), from_folder)
            self.assertEqual(posixpath.normpath(posixpath.join('/', entry['Path'])), TRACKS[0xa1][1])
        self.assertEqual(playlist[2]['LibraryItemId'], second, 'the relative path outranks LibraryItemId')
        self.assertEqual(playlist[4], {'Path': TRACKS[0xa1][1], 'Type': 'Manual', 'ItemId': second})
        self.assertEqual([sorted(entry) for entry in pathless[1:]], [['LibraryItemId', 'Type']] * 2)
        self.assertEqual([entry['LibraryItemId'] in items for entry in pathless[1:]], [True, False])
        self.assertEqual(linked['collection']['members'], [rated, allowed, locked])
        self.assertEqual(linked['playlist']['members'], [second, second, first, second])
        self.assertEqual(linked['pathless']['members'], [first, second])

    def test_a_linked_container_without_its_live_member_cannot_be_seeded(self):
        # A collection's first refresh can reload an empty collection.xml.
        source = self.dir / 'empty.db'
        manifest = canned_10_11_8(source)
        self.sql(source, "UPDATE BaseItems SET Data = '{\"LinkedChildren\":[]}' WHERE Name = 'linked_collection'")
        with self.assertRaisesRegex(CheckError, 'the linked collection has no live member'):
            seed_database(source, manifest)

    def test_the_seed_waits_until_10_11_8_has_saved_each_live_member(self):
        # 10.11.8 first saves a new playlist's member with its Path only; the ItemId comes
        # with the queued refresh, which no API reports (gl-603).
        source = self.dir / 'pending.db'
        manifest = canned_10_11_8(source)
        member = uuid.UUID(guid(0xa2)).hex
        self.sql(source, "UPDATE BaseItems SET Data = json_remove(Data, '$.LinkedChildren[0].ItemId') "
                         "WHERE Name = 'linked_playlist'")
        with self.assertRaisesRegex(CheckError, 'the linked playlist has no live member after 0 s'):
            wait_for_live_members(source, manifest, timeout=0)
        refresh = threading.Timer(0.2, self.sql, (source, "UPDATE BaseItems SET Data = json_set(Data, "
                                  "'$.LinkedChildren[0].ItemId', ?) WHERE Name = 'linked_playlist'", member))
        refresh.start()
        self.addCleanup(refresh.cancel)
        wait_for_live_members(source, manifest, timeout=30, interval=0.1)

    def test_a_linked_container_is_checked_only_where_the_source_held_its_seeds(self):
        pathless = self.seeds['linked_children']['pathless']
        # 10.11.9+'s startup clean-up dropped the path-less entries before Ferrofin saw them,
        # and its save of the container left the live entry without its cached id.
        dropped = copy.deepcopy(self.state)
        dropped['linked_children']['pathless'] = [{'Path': pathless['entries'][0]['Path'], 'Type': 'Manual'}]
        validate_seed_state(dropped, self.seeds, '10.11.9')
        with self.assertRaisesRegex(CheckError, 'did not keep the seeded pathless LinkedChildren'):
            validate_seed_state(self.state, self.seeds, '10.11.9')
        # Elsewhere a re-cached id still fails: only the container the clean-up saves is spared.
        recached = copy.deepcopy(dropped)
        del recached['linked_children']['playlist'][1]['ItemId']
        with self.assertRaisesRegex(CheckError, 'did not keep the seeded playlist LinkedChildren'):
            validate_seed_state(recached, self.seeds, '10.11.9')
        with self.assertRaisesRegex(CheckError, 'did not keep the seeded pathless LinkedChildren'):
            validate_seed_state(dropped, self.seeds, '10.11.8')
        self.sql(self.adopted, 'DELETE FROM LinkedChildren WHERE ParentId = ? AND SortOrder = 1',
                 str(uuid.UUID(pathless['id'])).upper())
        self.actual['linked_children']['pathless'] = pathless['members'][:1]
        self.assertEqual(self.failures(), ['seeded pathless LinkedChildren differ from Jellyfin 12.2',
                                           'seeded pathless LinkedChildren are served differently from Jellyfin 12.2'])
        self.state = dropped
        self.assertEqual(self.failures(), [])
        # A 12.x source holds the migrated members, so the container is checked there too.
        self.state = seed_state(self.oracle_db, self.seeds)
        self.assertEqual(self.failures(), ['seeded pathless LinkedChildren differ from Jellyfin 12.2',
                                           'seeded pathless LinkedChildren are served differently from Jellyfin 12.2'])

    def test_a_linked_child_resolved_unlike_jellyfin_12_2_fails(self):
        collection, playlist = (str(uuid.UUID(self.seeds['linked_children'][role]['id'])).upper()
                                for role in ('collection', 'playlist'))
        locked = guid(MOVIES.index('locked_movie') + 1)
        # The LibraryItemId fallback ignored, and the playlist's repeat collapsed.
        for statement, args, failure in (
                ('DELETE FROM LinkedChildren WHERE ParentId = ? AND ChildId = ?', (collection, locked), 'collection'),
                ('DELETE FROM LinkedChildren WHERE ParentId = ? AND SortOrder = ?', (playlist, 1), 'playlist')):
            with self.subTest(failure=failure):
                self.adopted.unlink()
                shutil.copyfile(self.oracle_db, self.adopted)
                self.sql(self.adopted, statement, *args)
                self.assertEqual(self.failures(), [f'seeded {failure} LinkedChildren differ from Jellyfin 12.2'])
        shutil.copyfile(self.oracle_db, self.adopted)
        # Order is membership for a playlist.
        self.sql(self.adopted, 'UPDATE LinkedChildren SET SortOrder = 3 - SortOrder WHERE ParentId = ?', playlist)
        self.assertEqual(self.failures(), ['seeded playlist LinkedChildren differ from Jellyfin 12.2'])
        shutil.copyfile(self.oracle_db, self.adopted)
        self.actual['linked_children']['playlist'] = self.actual['linked_children']['playlist'][1:]
        self.assertEqual(self.failures(), ['seeded playlist LinkedChildren are served differently from Jellyfin 12.2'])

    def test_seeding_twice_changes_nothing(self):
        with closing(sqlite3.connect(self.source)) as conn:
            dump = list(conn.iterdump())
        self.assertEqual(seed_database(self.source, self.manifest), self.seeds)
        with closing(sqlite3.connect(self.source)) as conn:
            self.assertEqual(list(conn.iterdump()), dump)

    def test_an_adoption_that_matches_jellyfin_12_2_passes(self):
        self.assertEqual(self.failures(), [])

    def test_the_12_0_lineage_drops_the_chain_and_keeps_the_other_seeds(self):
        # Jellyfin 12.0 cannot migrate a database holding the chain, so its copy drops it.
        with closing(sqlite3.connect(self.source)) as conn:
            before = conn.execute('SELECT COUNT(*) FROM BaseItems').fetchone()[0]
        drop_dangling_chain(self.source, self.seeds)
        with closing(sqlite3.connect(self.source)) as conn:
            self.assertEqual(orphaned_items(conn), set())
            self.assertEqual(conn.execute('SELECT COUNT(*) FROM BaseItems').fetchone()[0], before - 2)
        state = seed_state(self.source, self.seeds)
        self.assertEqual((state['orphans'], state['history']), ([], None))
        self.assertEqual(state['ratings'], self.seeds['ratings'])
        self.assertEqual(sorted(state['conflict']), sorted(self.seeds['conflict']['keys']))
        drop_dangling_chain(self.source, self.seeds)  # idempotent
        self.assertEqual(seed_state(self.source, self.seeds), state)

    def test_a_12_0_lineage_adoption_is_checked_only_for_survivors(self):
        with_chain = self.dir / 'with-chain.db'
        shutil.copyfile(self.source, with_chain)
        drop_dangling_chain(self.source, self.seeds)
        self.state = seed_state(self.source, self.seeds)
        # Adopted from the chain-free source: no placeholder history exists, and none is demanded.
        shutil.copyfile(self.source, self.adopted)
        jellyfin_12_2(self.adopted, self.seeds)
        self.assertEqual(self.failures(), [])
        # A chain item that survives is still caught.
        orphan = str(uuid.UUID(self.seeds['orphans'][0])).upper()
        with closing(sqlite3.connect(self.adopted)) as conn, conn:
            conn.execute('ATTACH ? AS chain', (str(with_chain),))
            conn.execute('INSERT INTO BaseItems SELECT * FROM chain.BaseItems WHERE Id = ?', (orphan,))
        self.assertEqual(self.failures(), ['seeded orphaned items survive'])

    def test_a_score_from_older_tables_fails(self):
        # Jellyfin 12.1 scored "-12" as -12; "SAM 13" was unrated before 12.2.
        for rating, value in (('-12', -12), ('SAM 13', None)):
            with self.subTest(rating=rating):
                self.sql(self.adopted, 'UPDATE BaseItems SET InheritedParentalRatingValue = ? WHERE OfficialRating = ?',
                         value, rating)
                self.assertEqual(self.failures(), ['seeded ratings differ from Jellyfin 12.2'])
                self.sql(self.adopted, 'UPDATE BaseItems SET InheritedParentalRatingValue = ? WHERE OfficialRating = ?',
                         SCORES[rating][0], rating)

    def test_a_visibility_change_fails(self):
        self.actual['visibility'][USERS[2]].append(
            next(movie for movie, rating in self.seeds['ratings'].items() if rating == 'FSK 16'))
        self.assertEqual(self.failures(), ['seeded rating visibility differs from Jellyfin 12.2'])

    def test_unharmonized_or_differently_harmonized_user_data_fails(self):
        self.sql(self.adopted, "UPDATE UserData SET Played = 0, PlaybackPositionTicks = 490 WHERE CustomDataKey = 'tt99999991'")
        self.assertEqual(self.failures(), ['seeded conflicting user data differs from Jellyfin 12.2'])

    def test_the_same_instant_written_differently_passes(self):
        self.sql(self.adopted, "UPDATE UserData SET LastPlayedDate = '2023-08-14 00:00:00.0000000' "
                 "WHERE CustomDataKey = 'tt99999991'")
        self.assertEqual(self.failures(), [])

    def test_a_surviving_orphan_fails(self):
        self.sql(self.adopted, "INSERT INTO BaseItems (Id, Type) VALUES (?, 'x')",
                 str(uuid.UUID(self.seeds['orphans'][1])).upper())
        self.assertEqual(self.failures(), ['seeded orphaned items survive'])

    def test_orphaned_history_must_stay_on_the_placeholder_with_a_retention_date(self):
        key = self.seeds['orphan_history']['key']
        for statement in ('UPDATE UserData SET RetentionDate = NULL WHERE CustomDataKey = ?',
                          'DELETE FROM UserData WHERE CustomDataKey = ?'):
            with self.subTest(statement=statement):
                self.adopted.unlink()
                shutil.copyfile(self.oracle_db, self.adopted)
                self.sql(self.adopted, statement, key)
                self.assertEqual(self.failures(), ['seeded orphaned history is not on the placeholder item'])

    def test_the_oracle_must_have_scored_every_seeded_rating(self):
        validate_oracle(self.oracle_db, self.manifest)
        with self.assertRaisesRegex(CheckError, 'lacks the database seeds'):
            validate_oracle(self.oracle_db, {})
        # NULL is unscored; so is 0, the stale 10.11.8 value of a movie that was G.
        for value in (None, 0):
            with self.subTest(value=value):
                self.sql(self.oracle_db, "UPDATE BaseItems SET InheritedParentalRatingValue = ? "
                         "WHERE OfficialRating = 'SAM 13'", value)
                with self.assertRaisesRegex(CheckError, 'unscored'):
                    validate_oracle(self.oracle_db, self.manifest)

    def test_the_seed_state_records_what_each_release_left(self):
        self.assertEqual(self.state['ratings'], self.seeds['ratings'])
        self.assertEqual(sorted(self.state['conflict']), sorted(self.seeds['conflict']['keys']))
        self.assertEqual(self.state['orphans'], sorted(self.seeds['orphans']))
        self.assertEqual(self.state['history'], 'item')
        linked = self.seeds['linked_children']
        self.assertEqual(self.state['linked_children'], {role: seed['entries'] for role, seed in linked.items()})
        validate_seed_state(self.state, self.seeds, '10.11.8')
        oracle = seed_state(self.oracle_db, self.seeds)
        self.assertEqual((oracle['orphans'], oracle['history']), ([], 'placeholder'))
        self.assertEqual(oracle['linked_children'], {role: seed['members'] for role, seed in linked.items()})
        validate_seed_state(oracle, self.seeds, '12.2')
        # The state survives its JSON file unchanged.
        self.assertEqual(json.loads(json.dumps(oracle)), oracle)

    def test_a_release_that_lost_a_seed_fails_its_build(self):
        harmonized = seed_state(self.oracle_db, self.seeds)
        cases = [
            ('10.11.9', {**self.state, 'ratings': {**self.state['ratings'], self.manifest['allowed_movie']: 'G'}},
             'did not keep the seeded ratings'),
            ('10.11.9', {**self.state, 'conflict': dict(list(self.state['conflict'].items())[:1])},
             'did not keep the seeded conflicting user data'),
            ('12.1', harmonized, 'changed the seeded conflicting user data'),
            ('12.2', self.state, 'did not harmonize'),
        ]
        # A 10.11 save that re-caches a stale id from its path, and a 12.x that dropped a member.
        recached = copy.deepcopy(self.state)
        recached['linked_children']['collection'][1]['ItemId'] = recached['linked_children']['collection'][0]['ItemId']
        unresolved = copy.deepcopy(harmonized)
        unresolved['linked_children']['playlist'].pop()
        cases += [('10.11.10', recached, 'did not keep the seeded collection LinkedChildren'),
                  ('12.2', unresolved, 'did not resolve the seeded playlist LinkedChildren')]
        for version, state, message in cases:
            with self.subTest(message=message), self.assertRaisesRegex(CheckError, message):
                validate_seed_state(state, self.seeds, version)

    def test_the_chain_check_applies_only_where_the_source_still_had_the_history(self):
        # An older release's clean-up may have dropped the second orphan's history before
        # Ferrofin ever saw it; that is not Ferrofin's loss.
        self.sql(self.adopted, 'DELETE FROM UserData WHERE CustomDataKey = ?', self.seeds['orphan_history']['key'])
        self.assertEqual(self.failures(), ['seeded orphaned history is not on the placeholder item'])
        self.state['history'] = None
        self.assertEqual(self.failures(), [])
        # Already detached in the source (Jellyfin's own clean-up): still required.
        self.state['history'] = 'placeholder'
        self.assertEqual(self.failures(), ['seeded orphaned history is not on the placeholder item'])

    def test_a_seeded_movies_resume_membership_follows_the_oracle(self):
        self.actual['views'][USERS[2]]['resume'] = []
        self.assertEqual(self.failures(), ['seeded movie resume differs from Jellyfin 12.2'])
        self.actual = copy.deepcopy(self.oracle)
        # The unharmonized loser (position 490, unplayed) would make the conflict resumable.
        self.actual['views'][USERS[1]]['resume'].append(self.seeds['conflict']['item'])
        self.assertEqual(self.failures(), ['seeded movie resume differs from Jellyfin 12.2'])

    def test_the_chain_is_written_even_where_foreign_keys_are_enforced(self):
        source = self.dir / 'enforced.db'
        manifest = canned_10_11_8(source)
        real = sqlite3.connect

        def enforcing(*args, **kwargs):
            conn = real(*args, **kwargs)
            conn.execute('PRAGMA foreign_keys = ON')
            return conn

        with patch.object(synthetic_fixture.sqlite3, 'connect', enforcing):
            seeds = seed_database(source, manifest)
        with closing(sqlite3.connect(source)) as conn:
            self.assertEqual(orphaned_items(conn), set(seeds['orphans']))
            self.assertEqual(len(conn.execute('PRAGMA foreign_key_check').fetchall()), 1)

    def test_per_release_comparison_leaves_seeded_items_to_the_oracle(self):
        movie, orphan = self.seeds['conflict']['item'], self.seeds['orphans'][1]
        allowed = self.manifest['allowed_movie']
        expected = {'visibility': {USERS[2]: [movie, 'other']},
                    'views': {USERS[1]: {'resume': [movie, 'episode']}, USERS[2]: {'resume': [allowed]}},
                    'relationships': {orphan: {'Type': 'Episode'}, movie: {'Type': 'Movie'}}}
        actual = {'visibility': {USERS[2]: ['other']},
                  'views': {USERS[1]: {'resume': ['episode']}, USERS[2]: {'resume': [allowed]}},
                  'relationships': {movie: {'Type': 'Movie'}}}
        # The linked containers follow Jellyfin 12.2, never the source release.
        expected['linked_children'] = {'playlist': ['as 10.11 serves them']}
        actual['linked_children'] = {'playlist': ['as 12.2 resolves them']}
        orphans = set(self.seeds['orphans'])
        self.assertEqual(compare(comparable(expected, self.seeds, orphans), comparable(actual, self.seeds, orphans)), [])
        self.assertIn('linked_children', expected, 'the snapshot itself is not modified')
        # The seeded movie itself must survive; only its visibility follows Jellyfin 12.2.
        del actual['relationships'][movie]
        self.assertEqual(compare(comparable(expected, self.seeds, orphans), comparable(actual, self.seeds, orphans)),
                         ['relationships'])
        actual['relationships'][movie] = {'Type': 'Movie'}
        actual['visibility'][USERS[2]] = []
        self.assertEqual(compare(comparable(expected, self.seeds, orphans), comparable(actual, self.seeds, orphans)),
                         ['visibility'])
        # Only the conflicting movie leaves the views: the child's resume of the allowed
        # (seeded-rating) movie is still compared release by release.
        actual['visibility'][USERS[2]] = ['other']
        actual['views'][USERS[2]]['resume'] = []
        self.assertEqual(compare(comparable(expected, self.seeds, orphans), comparable(actual, self.seeds, orphans)),
                         ['views'])

    def test_the_matrix_refuses_to_run_without_the_12_2_oracle(self):
        # Checked before any container starts or any fixture is copied.
        with self.assertRaisesRegex(CheckError, 'oracle missing; build ' + ORACLE):
            run_matrix(self.dir, 'unused-image', None, 0)
        self.assertFalse((self.dir / 'work').exists())

    def test_the_matrix_refuses_to_run_without_the_real_10_11_upgrade(self):
        from synthetic import recipe
        oracle = self.dir / ORACLE
        oracle.mkdir()
        (oracle / 'synthetic-ready.json').write_text(json.dumps({'recipe': recipe()}))
        (oracle / 'preservation.json').write_text('{}')
        with self.assertRaisesRegex(CheckError, 'build jellyfin-12.2-from-10 first'):
            run_matrix(self.dir, 'unused-image', None, 0)
        self.assertFalse((self.dir / 'work').exists())

    def test_every_release_maps_to_its_adopted_generation(self):
        self.assertEqual({version: GENERATIONS[version] for _, version, _ in VERSIONS},
                         {'10.11.8': '10.11.8', '10.11.9': '10.11.8', '10.11.10': '10.11.11',
                          '10.11.11': '10.11.11', '12.0': '12.0.0', '12.1': '12.1.0', '12.2': '12.2.0'})

    def test_the_12_2_paths_upgrade_from_their_contract_parents(self):
        paths = {name: parent for name, _, parent in VERSIONS}
        self.assertEqual({name: paths[name] for name in paths if name.startswith('jellyfin-12.2')},
                         {'jellyfin-12.2-from-10': 'jellyfin-10.11.8', 'jellyfin-12.2-from-12': 'jellyfin-12.0',
                          'jellyfin-12.2-from-121': 'jellyfin-12.1-from-12'})
        names = [name for name, _, _ in VERSIONS]
        self.assertTrue(all(names.index(parent) < names.index(name) for name, parent in paths.items() if parent))
        self.assertIn(ORACLE, paths)


if __name__ == '__main__':
    unittest.main()
