"""Seed and verify invented user accounts through Jellyfin's normal APIs."""

import base64
from contextlib import closing
import hashlib
from io import BytesIO
import json
import sqlite3
import uuid

from PIL import Image

from metadata import CheckError
from watch_history import timestamp


ACCOUNT_NAMES = {role: 'synthetic-' + role for role in ('disabled', 'locked', 'passwordless', 'settings')}
# Distinct, generated avatars make a swapped user's image detectable.
AVATARS = {'synthetic-admin': 'red', 'synthetic-adult': 'lime', 'synthetic-child': 'blue',
           **dict(zip(ACCOUNT_NAMES.values(), ('cyan', 'magenta', 'yellow', 'orange')))}
TABLES = ('Users', 'Permissions', 'Preferences', 'AccessSchedules', 'ImageInfos')


def normalize(value):
    if isinstance(value, dict):
        return {key: normalize(val) for key, val in value.items()}
    if isinstance(value, list):
        return [normalize(val) for val in value]
    if isinstance(value, str):
        try:
            return uuid.UUID(value).hex
        except ValueError:
            pass
    return value


def seed(admin, manifest):
    from synthetic_fixture import Api, PASSWORD
    accounts = {}
    for role, name in ACCOUNT_NAMES.items():
        row = admin.post('/Users/New', {'Name': name, 'Password': '' if role == 'passwordless' else PASSWORD})
        accounts[role] = row['Id']
        manifest['users'][name] = row['Id']
    manifest['account_cases'] = accounts
    folders = list(manifest['libraries'].values())
    for role in ('disabled', 'locked'):
        uid = accounts[role]
        policy = admin.get(f'/Users/{uid}')['Policy']
        policy.update({'IsDisabled': role == 'disabled', 'LoginAttemptsBeforeLockout': 2})
        admin.post(f'/Users/{uid}/Policy', policy)
    anonymous = Api(admin.base)
    for _ in range(2):
        anonymous.call('POST', '/Users/AuthenticateByName',
                       {'Username': ACCOUNT_NAMES['locked'], 'Pw': 'Deliberately-wrong-synthetic-password'},
                       allowed=(401, 403))
    # A dedicated account exercises settings without changing the movie/history
    # scenarios or making their logins dependent on the current day or time.
    uid = accounts['settings']
    user = admin.get(f'/Users/{uid}')
    policy = user['Policy']
    for key, value in list(policy.items()):
        if isinstance(value, bool) and key not in ('IsAdministrator', 'IsDisabled', 'EnableLyricManagement'):
            policy[key] = not value
    policy.update({'MaxParentalRating': 12, 'MaxParentalSubRating': 2,
        'BlockedTags': ['Synthetic blocked'], 'AllowedTags': ['Synthetic allowed'],
        'BlockUnratedItems': ['Movie', 'Music'], 'EnabledDevices': ['synthetic-device'],
        'EnabledChannels': ['00000000-0000-0000-0000-00000000ca01'],
        'EnabledFolders': folders[:2], 'EnableContentDeletionFromFolders': folders[1:2],
        'AccessSchedules': [{'UserId': uid, 'DayOfWeek': 'Weekend', 'StartHour': 8.5, 'EndHour': 20.25}],
        'InvalidLoginAttemptCount': 1, 'LoginAttemptsBeforeLockout': 7,
        'MaxActiveSessions': 3, 'RemoteClientBitrateLimit': 4_000_000, 'SyncPlayAccess': 'JoinGroups'})
    admin.post(f'/Users/{uid}/Policy', policy)
    configuration = user['Configuration']
    for key, value in list(configuration.items()):
        if isinstance(value, bool):
            configuration[key] = not value
    configuration.update({'AudioLanguagePreference': 'deu', 'SubtitleLanguagePreference': 'spa',
        'SubtitleMode': 'OnlyForced', 'OrderedViews': list(reversed(folders)),
        'GroupedFolders': folders[:2], 'MyMediaExcludes': folders[1:2], 'LatestItemsExcludes': folders[2:3]})
    # Register a second real configuration entry rather than inventing an ID
    # that Jellyfin would silently discard when saving the user's preference.
    server_config = admin.get('/System/Configuration')
    server_config.setdefault('CastReceiverApplications', []).append(
        {'Id': 'synthetic-cast-receiver', 'Name': 'Synthetic cast receiver'})
    admin.post('/System/Configuration', server_config)
    configuration['CastReceiverId'] = 'synthetic-cast-receiver'
    admin.post('/Users/Configuration', configuration, userId=uid)
    # The public user list must include visible accounts and exclude hidden or
    # disabled ones; compare Jellyfin's actual list at every stage.
    for role in ('passwordless', 'disabled', 'locked'):
        uid = accounts[role]
        policy = admin.get(f'/Users/{uid}')['Policy']
        policy['IsHidden'] = False
        admin.post(f'/Users/{uid}/Policy', policy)
    for name, uid in manifest['users'].items():
        picture = BytesIO()
        Image.new('RGB', (80, 64), AVATARS[name]).save(picture, format='PNG')
        admin.call('POST', '/UserImage', raw=base64.b64encode(picture.getvalue()), userId=uid)


def image_state(payload):
    with Image.open(BytesIO(payload)) as picture:
        rgb = picture.convert('RGB')
        return {'size': list(rgb.size), 'pixels': hashlib.sha256(rgb.tobytes()).hexdigest()}


def snapshot(admin, manifest):
    from synthetic_fixture import Api, PASSWORD, USERS
    anonymous = Api(admin.base)
    users = {row['Name']: row for row in admin.get('/Users')}
    if set(users) != set(manifest['users']):
        raise CheckError('account list changed (missing, extra, or renamed users)')
    result = {'users': {}, 'avatars': {}, 'login': {},
              'public': sorted(row['Id'] for row in anonymous.get('/Users/Public'))}
    for name, uid in manifest['users'].items():
        row = users[name]
        if normalize(row['Id']) != normalize(uid):
            raise CheckError('account identity changed')
        # These complete objects deliberately have no field allowlist. A new
        # upstream policy/configuration field is automatically part of the gate.
        # Deprecated HasPassword DTO flags are not an authentication oracle;
        # Jellyfin now always reports true, even for passwordless accounts.
        result['users'][name] = {key: row.get(key) for key in ('Id', 'Name', 'EnableAutoLogin', 'Policy', 'Configuration')}
        result['users'][name]['HasAvatar'] = bool(row.get('PrimaryImageTag'))
        result['avatars'][name] = image_state(anonymous.call('GET', '/UserImage', binary=True, userId=uid, format='Png'))
    for name in (*USERS, ACCOUNT_NAMES['passwordless']):
        device = 'account-login-' + name
        response = Api(admin.base, device=device).post('/Users/AuthenticateByName',
                                  {'Username': name, 'Pw': '' if name == ACCOUNT_NAMES['passwordless'] else PASSWORD})
        if not response.get('AccessToken') or normalize(response['User']['Id']) != normalize(manifest['users'][name]):
            raise CheckError('account login did not return the original user identity and a token')
        session = Api(admin.base, response['AccessToken'], response['User']['Id'], device=device)
        result['login'][name] = session.get('/Users/Me')['Id']
    for role in ('disabled', 'locked'):
        result['login'][role] = anonymous.call('POST', '/Users/AuthenticateByName',
            {'Username': ACCOUNT_NAMES[role], 'Pw': PASSWORD}, allowed=(401, 403))['status']
        result['login'][role + '_wrong_password'] = anonymous.call('POST', '/Users/AuthenticateByName',
            {'Username': ACCOUNT_NAMES[role], 'Pw': 'Deliberately-wrong-synthetic-password'}, allowed=(401, 403))['status']
    return normalize(result)


def validate(data, manifest):
    users = data['users']
    disabled = users[ACCOUNT_NAMES['disabled']]['Policy']
    locked = users[ACCOUNT_NAMES['locked']]['Policy']
    settings = users[ACCOUNT_NAMES['settings']]
    if not disabled['IsDisabled'] or disabled['InvalidLoginAttemptCount'] != 0:
        raise CheckError('fixture lacks an administratively disabled account')
    if not locked['IsDisabled'] or locked['InvalidLoginAttemptCount'] < locked['LoginAttemptsBeforeLockout']:
        raise CheckError('fixture lacks an account locked by failed authentication')
    if not settings['Policy']['AccessSchedules'] or not settings['Configuration']['OrderedViews']:
        raise CheckError('fixture lacks populated account settings')
    if settings['Configuration'].get('CastReceiverId') != 'synthetic-cast-receiver':
        raise CheckError('fixture lacks a selected cast receiver')
    for name, row in users.items():
        if not row['HasAvatar']:
            raise CheckError('fixture lacks a profile image tag')
        expected = Image.new('RGB', (80, 64), AVATARS[name])
        if data['avatars'][name] != {'size': [80, 64], 'pixels': hashlib.sha256(expected.tobytes()).hexdigest()}:
            raise CheckError('fixture has missing, incorrect, or swapped profile images')
    passwordless = normalize(manifest['account_cases']['passwordless'])
    if data['login'].get(ACCOUNT_NAMES['passwordless']) != passwordless or passwordless not in data['public']:
        raise CheckError('fixture lacks a visible, usable passwordless account')
    if any(normalize(manifest['account_cases'][role]) in data['public'] for role in ('disabled', 'locked')):
        raise CheckError('fixture exposes a disabled account in the public user list')


def database_snapshot(db):
    """Hash stable logical account rows; never retain password hashes in reports."""
    result = {}
    with closing(sqlite3.connect(f'file:{db}?mode=ro', uri=True)) as conn:
        conn.row_factory = sqlite3.Row
        conn.execute('BEGIN')
        for table in TABLES:
            rows = []
            for record in conn.execute(f'SELECT * FROM "{table}"'):
                row = dict(record)
                # Jellyfin upgrades can leave detached permission/preference
                # rows. Migration 0032 removes these; they belong to no user.
                # Losing the owner of a previously attached row still changes
                # the snapshot because that original row disappears.
                if table in ('Permissions', 'Preferences') and row['UserId'] is None:
                    continue
                # EF bookkeeping is not account state. NormalizedUsername is
                # introduced by adoption and derives from the preserved name.
                for key in ('RowVersion', 'NormalizedUsername', 'Permission_Permissions_Guid', 'Preference_Preferences_Guid'):
                    row.pop(key, None)
                if table == 'Users':
                    for key in ('LastLoginDate', 'LastActivityDate'):
                        row.pop(key, None)  # successful login probes advance these
                else:
                    row.pop('Id', None)
                if table == 'Preferences':
                    row['Value'] = [normalize(value) for value in row['Value'].split(',')] if row['Value'] else []
                if table == 'ImageInfos':
                    row['LastModified'] = timestamp(row['LastModified'])
                rows.append(hashlib.sha256(json.dumps(normalize(row), sort_keys=True).encode()).hexdigest())
            result[table] = sorted(rows)
    return result


def compare_database(before, after):
    return ['account database ' + table + ' changed' for table in before if before[table] != after.get(table)]
