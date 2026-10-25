"""Account migration checks fail on lost state, broken login, and unusable avatars."""

import copy
from io import BytesIO
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

from PIL import Image

sys.path.insert(0, str(Path(__file__).parents[1]))
from metadata import CheckError
from preservation import compare
from synthetic_fixture import PASSWORD, USERS
from user_accounts import ACCOUNT_NAMES, AVATARS, TABLES, database_snapshot, compare_database, image_state, snapshot, validate


def fixture():
    names = [*USERS, *ACCOUNT_NAMES.values()]
    manifest = {'users': {name: f'{i:032x}' for i, name in enumerate(names, 1)}}
    manifest['account_cases'] = {role: manifest['users'][name] for role, name in ACCOUNT_NAMES.items()}
    data = {'users': {}, 'avatars': {}, 'login': {}, 'public': [manifest['account_cases']['passwordless']]}
    for name, uid in manifest['users'].items():
        data['users'][name] = {'Id': uid, 'Name': name, 'HasAvatar': True,
            'Policy': {'IsDisabled': False, 'InvalidLoginAttemptCount': 0, 'LoginAttemptsBeforeLockout': 2,
                       'AccessSchedules': [], 'FuturePolicyField': ['preserve me']},
            'Configuration': {'OrderedViews': [], 'FutureConfigurationField': True}}
        image = BytesIO()
        Image.new('RGB', (80, 64), AVATARS[name]).save(image, format='PNG')
        data['avatars'][name] = image_state(image.getvalue())
    for role in ('disabled', 'locked'):
        data['users'][ACCOUNT_NAMES[role]]['Policy']['IsDisabled'] = True
        data['login'][role] = 403
    data['users'][ACCOUNT_NAMES['locked']]['Policy']['InvalidLoginAttemptCount'] = 2
    settings = data['users'][ACCOUNT_NAMES['settings']]
    settings['Policy']['AccessSchedules'] = [{'DayOfWeek': 'Weekend', 'StartHour': 8.5, 'EndHour': 20.25}]
    settings['Configuration'].update({'OrderedViews': ['second', 'first'], 'CastReceiverId': 'synthetic-cast-receiver'})
    data['login'].update({name: manifest['users'][name] for name in (*USERS, ACCOUNT_NAMES['passwordless'])})
    return manifest, data


class AccountValidationTest(unittest.TestCase):
    def setUp(self):
        self.manifest, self.data = fixture()

    def test_complete_account_scenarios_pass(self):
        validate(self.data, self.manifest)

    def test_disabled_account_must_really_be_disabled(self):
        self.data['users'][ACCOUNT_NAMES['disabled']]['Policy']['IsDisabled'] = False
        with self.assertRaisesRegex(CheckError, 'administratively disabled'):
            validate(self.data, self.manifest)

    def test_lockout_must_come_from_failed_authentication(self):
        self.data['users'][ACCOUNT_NAMES['locked']]['Policy']['InvalidLoginAttemptCount'] = 0
        with self.assertRaisesRegex(CheckError, 'locked by failed authentication'):
            validate(self.data, self.manifest)

    def test_passwordless_account_must_authenticate(self):
        self.data['login'][ACCOUNT_NAMES['passwordless']] = 'wrong identity'
        with self.assertRaisesRegex(CheckError, 'passwordless'):
            validate(self.data, self.manifest)

    def test_disabled_users_must_not_be_public(self):
        self.data['public'].append(self.manifest['account_cases']['disabled'])
        with self.assertRaisesRegex(CheckError, 'public user list'):
            validate(self.data, self.manifest)

    def test_profile_image_tag_is_required(self):
        self.data['users'][USERS[0]]['HasAvatar'] = False
        with self.assertRaisesRegex(CheckError, 'profile image tag'):
            validate(self.data, self.manifest)

    def test_swapped_avatars_are_detected(self):
        self.data['avatars'][USERS[0]] = self.data['avatars'][USERS[1]]
        with self.assertRaisesRegex(CheckError, 'swapped profile'):
            validate(self.data, self.manifest)

    def test_corrupt_avatar_is_rejected(self):
        with self.assertRaises(OSError):
            image_state(b'not an image')

    def test_empty_schedules_cannot_pass(self):
        self.data['users'][ACCOUNT_NAMES['settings']]['Policy']['AccessSchedules'] = []
        with self.assertRaisesRegex(CheckError, 'populated account settings'):
            validate(self.data, self.manifest)

    def test_cast_receiver_must_be_selected(self):
        self.data['users'][ACCOUNT_NAMES['settings']]['Configuration']['CastReceiverId'] = None
        with self.assertRaisesRegex(CheckError, 'cast receiver'):
            validate(self.data, self.manifest)

    def test_all_policy_and_configuration_fields_are_compared(self):
        for section in ('Policy', 'Configuration'):
            for key in self.data['users'][ACCOUNT_NAMES['settings']][section]:
                with self.subTest(section=section, field=key):
                    changed = copy.deepcopy(self.data)
                    del changed['users'][ACCOUNT_NAMES['settings']][section][key]
                    self.assertEqual(compare({'accounts': self.data}, {'accounts': changed}), ['accounts'])

    def test_ordered_preferences_are_compared_in_order(self):
        changed = copy.deepcopy(self.data)
        changed['users'][ACCOUNT_NAMES['settings']]['Configuration']['OrderedViews'].reverse()
        self.assertEqual(compare({'accounts': self.data}, {'accounts': changed}), ['accounts'])


class AccountSnapshotTest(unittest.TestCase):
    def setUp(self):
        self.manifest, self.data = fixture()
        self.login_id = None
        self.token = 'synthetic-token'
        test = self
        class FakeApi:
            def __init__(self, base='synthetic', token=None, user=None, device=None):
                self.base, self.user = base, user
            def get(self, path):
                if path == '/Users':
                    return [dict(row, PrimaryImageTag='synthetic-image-tag') for row in test.data['users'].values()]
                if path == '/Users/Public':
                    return [{'Id': uid} for uid in test.data['public']]
                if path == '/Users/Me':
                    return {'Id': self.user}
                raise AssertionError(path)
            def post(self, path, body):
                test.assertEqual(path, '/Users/AuthenticateByName')
                name = body['Username']
                test.assertEqual(body['Pw'], '' if name == ACCOUNT_NAMES['passwordless'] else PASSWORD)
                return {'AccessToken': test.token, 'User': {'Id': test.login_id or test.manifest['users'][name]}}
            def call(self, method, path, body=None, **kwargs):
                if path == '/UserImage':
                    name = next(name for name, uid in test.manifest['users'].items() if uid == kwargs['userId'])
                    picture = BytesIO()
                    Image.new('RGB', (80, 64), AVATARS[name]).save(picture, format='PNG')
                    return picture.getvalue()
                test.assertEqual(kwargs['allowed'], (401, 403))
                return {'status': 403}
        self.api = FakeApi
        self.patch = patch('synthetic_fixture.Api', FakeApi)
        self.patch.start()
        self.addCleanup(self.patch.stop)

    def test_snapshot_keeps_complete_objects_including_future_fields(self):
        actual = snapshot(self.api(), self.manifest)
        for name in self.manifest['users']:
            for section in ('Policy', 'Configuration'):
                self.assertEqual(actual['users'][name][section], self.data['users'][name][section])
        validate(actual, self.manifest)

    def test_missing_account_fails_before_login(self):
        del self.data['users'][USERS[1]]
        with self.assertRaisesRegex(CheckError, 'account list changed'):
            snapshot(self.api(), self.manifest)

    def test_successful_login_must_return_the_same_identity(self):
        self.login_id = '00000000000000000000000000009999'
        with self.assertRaisesRegex(CheckError, 'original user identity'):
            snapshot(self.api(), self.manifest)

    def test_successful_login_requires_a_token(self):
        self.token = ''
        with self.assertRaisesRegex(CheckError, 'original user identity'):
            snapshot(self.api(), self.manifest)


class AccountDatabaseTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.db = Path(tmp.name) / 'test.db'
        self.sql('''CREATE TABLE Users(Id TEXT, Username TEXT, Password TEXT, InvalidLoginAttemptCount INTEGER,
            LastLoginDate TEXT, LastActivityDate TEXT, RowVersion INTEGER);
            INSERT INTO Users VALUES ('00000000-0000-0000-0000-000000000001','synthetic','invented hash',2,NULL,NULL,1);
            CREATE TABLE Permissions(Id INTEGER, UserId TEXT, Kind INTEGER, Value INTEGER);
            INSERT INTO Permissions VALUES (1,'00000000-0000-0000-0000-000000000001',2,1);
            CREATE TABLE Preferences(Id INTEGER, UserId TEXT, Kind INTEGER, Value TEXT);
            INSERT INTO Preferences VALUES (1,'00000000-0000-0000-0000-000000000001',0,'second,first');
            CREATE TABLE AccessSchedules(Id INTEGER,UserId TEXT,DayOfWeek INTEGER,StartHour REAL,EndHour REAL);
            INSERT INTO AccessSchedules VALUES (1,'00000000-0000-0000-0000-000000000001',7,8.5,20.25);
            CREATE TABLE ImageInfos(Id INTEGER,UserId TEXT,Path TEXT,LastModified TEXT);
            INSERT INTO ImageInfos VALUES (1,'00000000-0000-0000-0000-000000000001','/generated/avatar.png','2026-01-01 00:00:00');''')
        self.before = database_snapshot(self.db)

    def sql(self, statement):
        conn = sqlite3.connect(self.db)
        try:
            conn.executescript(statement)
        finally:
            conn.close()

    def test_unchanged_accounts_pass(self):
        self.assertFalse(compare_database(self.before, database_snapshot(self.db)))

    def test_lost_password_is_detected_without_exposing_hash(self):
        self.assertNotIn('invented hash', str(self.before))
        self.sql('UPDATE Users SET Password=NULL;')
        self.assertEqual(compare_database(self.before, database_snapshot(self.db)), ['account database Users changed'])

    def test_lockout_counter_reset_is_detected(self):
        self.sql('UPDATE Users SET InvalidLoginAttemptCount=0;')
        self.assertTrue(compare_database(self.before, database_snapshot(self.db)))

    def test_missing_rows_in_each_account_table_are_detected(self):
        for table in TABLES:
            with self.subTest(table=table):
                after = copy.deepcopy(self.before)
                after[table] = []
                self.assertEqual(compare_database(self.before, after), ['account database ' + table + ' changed'])

    def test_reassigned_permission_is_detected(self):
        for owner in ("'different-user'", 'NULL'):
            with self.subTest(owner=owner):
                self.sql(f'UPDATE Permissions SET UserId={owner};')
                self.assertTrue(compare_database(self.before, database_snapshot(self.db)))

    def test_unowned_legacy_rows_may_be_removed(self):
        self.sql('''INSERT INTO Permissions VALUES (2,NULL,0,1);
            INSERT INTO Preferences VALUES (2,NULL,0,'legacy');''')
        self.assertFalse(compare_database(self.before, database_snapshot(self.db)))

    def test_changed_preference_order_is_detected(self):
        self.sql("UPDATE Preferences SET Value='first,second';")
        self.assertTrue(compare_database(self.before, database_snapshot(self.db)))

    def test_changed_schedule_hours_are_detected(self):
        self.sql('UPDATE AccessSchedules SET EndHour=24;')
        self.assertTrue(compare_database(self.before, database_snapshot(self.db)))

    def test_login_timestamps_and_ef_bookkeeping_may_advance(self):
        self.sql("UPDATE Users SET LastLoginDate='2026-02-01',LastActivityDate='2026-02-01',RowVersion=2;")
        self.assertFalse(compare_database(self.before, database_snapshot(self.db)))

    def test_guid_and_image_date_representation_are_normalized(self):
        self.sql("UPDATE Users SET Id='00000000000000000000000000000001'; UPDATE ImageInfos SET LastModified='2026-01-01T00:00:00Z';")
        self.assertFalse(compare_database(self.before, database_snapshot(self.db)))


if __name__ == '__main__':
    unittest.main()
