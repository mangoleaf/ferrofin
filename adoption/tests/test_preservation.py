"""Regression tests for synthetic coverage and the unchanged-scan write audit."""

from contextlib import closing
import copy
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
from metadata import CheckError
from preservation import compare, normalize, validate, same_artwork, verify_database
from synthetic_fixture import USERS
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


if __name__ == '__main__':
    unittest.main()
