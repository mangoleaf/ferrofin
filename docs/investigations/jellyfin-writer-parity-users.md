# Writer parity: accounts and their state

Ledger for phase gl-1559 of the "Backwards migration to Jellyfin" milestone: every column Ferrofin writes in the account tables, compared with what Jellyfin 12.4 writes, with a verdict each. Under decision D8 the default verdict is to fix toward upstream. An accepted divergence needs the owner's explicit decision, so this ledger only proposes them. Code is cited at `release/v1.6.0` `978bea505`. Upstream is Jellyfin `v12.4` (`UPSTREAM_TAG`), cited relative to its checkout. The pinned schema is `crates/ferrofin-db/tests/data/jellyfin-12.2-schema.sql`.

**Tables (127 columns):** `Users` (32), `Permissions` (5), `Preferences` (5), `AccessSchedules` (5), `ImageInfos` (4, the profile image), `UserData` (13), `Devices` (11), `DeviceOptions` (3), `ApiKeys` (5), `ActivityLogs` (10), `DisplayPreferences` (14), `ItemDisplayPreferences` (10), `CustomItemDisplayPreferences` (6), `HomeSection` (4).

**Verdicts:**
- **match:** same value and text form.
- **fix:** Ferrofin differs. The row names the issue that tracks it, filed by this audit or already open.
- **recorded:** a divergence an owner decision accepts, or one the code records as a Jellyfin bug not ported; it is cited.
- **owner?:** a possible accepted divergence the owner has not decided. Each is proposed below (P1 to P6), or waits on its own issue. **owner? (documented)** is a deliberate Ferrofin difference a code comment explains, with no owner decision yet: listed together as P6.
- **match; edge fix** or **match; recorded edge:** the column matches except in an edge case, which the named issue tracks or the code records as a Jellyfin bug not ported.

## Method

**Static.** For each column, every Ferrofin writer was compared with upstream's writer at `v12.4`, by searching the table name for `INSERT`/`UPDATE`. The upstream writers are:
- the user manager and `User` entity;
- `UserDataManager`, `SessionManager` and `BaseItem`;
- `DeviceManager` and `AuthorizationContext`;
- `AuthenticationManager`;
- `ActivityManager` and its event consumers;
- `DisplayPreferencesManager` and `DisplayPreferencesController`;
- the EF model and its `DateTimeKindValueConverter`.

**Empirical.** One scratch run, built with the adoption gate's synthetic code (`jellyfin-12.4-fresh`: 7 seeded users covering admin, adult, child, disabled, locked, passwordless, and a "settings" account refused by its parental access schedule). The same scripted account flows ran on Jellyfin 12.4 (run J) and on Ferrofin adopting a copy (run F), both in the official `jellyfin/jellyfin:12.4` image:
- the login of every seeded account but the child;
- a wrong password;
- creating a user with a password, then that user changing it;
- a policy save and a configuration save;
- played, favorite and rating on one movie, and a playback start/stop at 300 s on another;
- a display-preferences save with home sections;
- an API key;
- a device's custom name;
- a logout.

Each run's 14 tables were dumped and compared row by row, matched on natural keys, with user ids mapped by name. Then Jellyfin 12.4 booted on a copy of F's output (run R) and every account logged in again.

**Results:**
- **Logins:** every account tried (all but the child) answered the same in J, F and R: 200 for admin, adult and passwordless, and 403 for disabled, locked and settings. A wrong password answered 401 in J and F (R did not try one). That includes the Ferrofin-created user whose password Ferrofin had changed, logging in on Jellyfin. **The password hash is compatible.**
- **R:** Jellyfin rewrote no account row it was not asked to. Only the logins' dates, devices and activity rows changed.
- **J against F:** `Permissions`, `Preferences`, `UserData`, `DeviceOptions`, `ItemDisplayPreferences`, `HomeSection` and the user rows matched value for value, apart from the datetime form and the differences below. Each one is backed by its static row.
- **Text forms**, from J's real rows:
  - EF writes GUIDs uppercase and hyphenated.
  - EF writes datetimes as `yyyy-MM-dd HH:mm:ss.FFFFFFF`, trailing zeros trimmed and the dot dropped on whole seconds (`2026-11-10 20:01:33.228654`, `2026-01-06 12:00:00`).
  - An API key's `DateLastActivity` is `0001-01-01 00:00:00`.

## Cross-cutting: datetime text form

`datetime_to_db` (`crates/ferrofin-db/src/store.rs:41-47`) always writes seven fractional digits, while EF's `FFFFFFF` trims them (J's rows above). Ferrofin records the EF form itself (`crates/ferrofin-core/src/adoption_repairs.rs:3390-3393`). The milestone's investigation gl-1565 (`PremiereDate` written with seven decimals) covers the same formatter, and its scope ends in a follow-up issue for the fix (D8). The rows below cite gl-1565 for that reason, and this audit added its evidence there.
- **Effect:** Jellyfin parses both forms, and text order holds. The one exception is that equal instants become unequal strings.
- **Affected columns:**
  - `Users.LastLoginDate`, `Users.LastActivityDate`;
  - `ImageInfos.LastModified`;
  - `UserData.LastPlayedDate`, `UserData.RetentionDate`;
  - `Devices.DateCreated`, `DateModified`, `DateLastActivity`;
  - `ApiKeys.DateCreated`;
  - `ActivityLogs.DateCreated`.

  Each of these rows below says "datetime form".
- **Verdict:** fix, through gl-1565's follow-up: one formatter. A Ferrofin save also rewrites an EF-written value into the seven-digit form; that is the same fix.

## Users, Permissions, Preferences, AccessSchedules, ImageInfos

Ferrofin: `crates/ferrofin-core/src/user_manager.rs` (UM), `user_manager/user_repository.rs` (UR), `user_entity_ext.rs` (UE), `access_schedule_repository.rs`, `user_activity_repository.rs`, `apps/ferrofin-server/src/seed.rs`. Upstream: `Jellyfin.Server.Implementations/Users/UserManager.cs` (J:UM), `src/Jellyfin.Database/Jellyfin.Database.Implementations/Entities/User.cs`, `Jellyfin.Data/UserEntityExtensions.cs` (J:UX).

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| Users.Id | UM:1020, 1045 | `User.cs:42` | match | uppercase GUID in both (J's rows) |
| Users.AudioLanguagePreference, SubtitleLanguagePreference, SubtitleMode, DisplayCollectionsView, DisplayMissingEpisodes, EnableLocalPassword, EnableNextEpisodeAutoPlay, HidePlayedInLatest, PlayDefaultAudioTrack, RememberAudioSelections, RememberSubtitleSelections | create UM:1034-1043; configuration UM:1864-1870 | constructor `User.cs:44-55`; J:UM:817-827 | match | same defaults and integers; J and F agreed after a configuration save |
| Users.AuthenticationProviderId, PasswordResetProviderId | UM:1046-1048; policy UM:919-920 | J:UM:326-327, 874-875 | match | |
| Users.EnableAutoLogin, MustUpdatePassword | create 0, never changed | `User.cs:45,52` | match | |
| Users.EnableUserPreferenceAccess, MaxActiveSessions, MaxParentalRatingScore, MaxParentalRatingSubScore, LoginAttemptsBeforeLockout, SyncPlayAccess | policy UM:892-920 | J:UM:858-879 | match | same -1/0 mapping for the lockout count |
| Users.InternalId | UM:994-998 | J:UM:320-330 | match | max + 1 |
| Users.Username, NormalizedUsername | UM:1049-1051, 1208-1213 | `User.cs:28-29`; J:UM:201-202 | match | upper-invariant |
| Users.Username (startup wizard) | `crates/ferrofin-api/src/handlers/startup.rs:306` | `Jellyfin.Api/Controllers/StartupController.cs:156` | match; edge fix (gl-1190) | ASCII-only case fold; upstream's is invariant-culture ("Émile" → "émile" renames in Ferrofin only) |
| Users.Username (bootstrap) | `apps/ferrofin-server/src/seed.rs:103`, `config.rs:52-53` | J:UM:756 `Environment.UserName` | owner? (P5) | flagged in the code, not decided: "FLAGGED for confirmation: `admin` is a convention" (`apps/ferrofin-server/src/config.rs:52`) |
| Users.CastReceiverId | create UM:1043 `'F007D354'` | constructor never sets it (`User.cs:22-56`) | fix (gl-1608) | Jellyfin-created users hold NULL in J's rows (admin, disabled, locked, passwordless) until a configuration save; Ferrofin stores `F007D354` at creation |
| Users.RemoteClientBitrateLimit | policy UM:919; fresh-install seed through `update_policy` (`apps/ferrofin-server/src/seed.rs:136-150`) | J:UM:873; bootstrap saves no policy (J:UM:745-772) | fix (gl-1608) | the seeded admin of a fresh Ferrofin install holds 0 where upstream's holds NULL |
| Users.InvalidLoginAttemptCount | UM:332, 361 (N+1 on a failure); 0 on success UM:1580 | J:UM:674, 680-697 | owner? (P1) | plain failures match. At lockout upstream counts the locking attempt twice (J:UM:680 `++` and saves, then `:697` `+ 1`): the fixture's locked user holds 3 with a limit of 2. Ferrofin writes N+1 |
| Users.Password | UM:1474-1481 → UR:73-75 | `DefaultAuthenticationProvider.cs:97-109`, `PasswordHash.cs:190-209` | match | same `$PBKDF2-SHA512$iterations=210000$…` form; run R logged in with a hash Ferrofin wrote |
| Users.Password (legacy hash on login) | UM:1556-1572 writes the upgraded hash | `DefaultAuthenticationProvider.cs:82-87` means to migrate it, but the user is reloaded `AsNoTracking` (J:UM:569-571) and the new hash is lost | owner? (P4) | a Jellyfin bug: upstream logs the migration on every login and never stores it. The comment at UM:1556-1558 claims upstream saves it, which v12.4 does not |
| Users.LastLoginDate, LastActivityDate | UM:1580-1584; `user_activity_repository.rs:23-28` | J:UM:658-663; `SessionManager.cs:267-272` | fix (gl-1565) | datetime form; same 60 s activity gate |
| Users.RowVersion | +1 on policy, configuration, password, activity, startup save, hash upgrade, seed | EF bumps Modified entities only (`JellyfinDbContext.cs:317`) | fix (gl-1607) | rename and lockout bump upstream but not here; the startup wizard, the hash upgrade and the seed bump here but not upstream. Harmless to Jellyfin, which reads what is stored |
| Permissions.Id, Kind, UserId, Value | UE:349-361, 194-206 | J:UX:187-213, 42-46 | match | |
| Permissions.RowVersion | inserted as 1 (UE:356, 204, 441), never bumped (UE:194, 431) | inserted 0, +1 on a changed value, +1 on all of a user's rows at lockout | fix (gl-1607) | J's new user: 0 and 1; F's: 1 everywhere |
| Preferences.Id, Kind, UserId, Value | UE:365-407 | J:UX:110-139, 219-225 | match | comma-joined values, lowercase "D" GUIDs |
| Preferences.RowVersion | inserted as 1 (UE:369, 404), not bumped by `set_preference_tx` | as Permissions | fix (gl-1607) | the view-consolidation repair bumps it as upstream does |
| AccessSchedules.UserId, DayOfWeek, StartHour, EndHour | `access_schedule_repository.rs:74-91` | J:UM:905-909 | match | |
| AccessSchedules.Id | renumbered on a policy save | renumbered on every `UpdateUserAsync` (J:UM:231-235), activity and image upload included | owner? (P2) | ids only; the values are identical |
| ImageInfos.Id, UserId, Path | UR:105-113; UM:1979-1995 | `ImageController.cs:129-145` | match | |
| ImageInfos.Path (rename) | UR:140 follows the rename | left as it was | recorded | `crates/ferrofin-api/src/handlers.rs:2891-2894` ("Jellyfin bug not ported": upstream then overwrites the renamed user's image); UM:1217-1219 |
| ImageInfos.LastModified | UR:109 | `ImageInfo.cs:19` | fix (gl-1565) | datetime form |

## UserData

Ferrofin: `crates/ferrofin-core/src/user_data_manager.rs` (udm), `user_data_keys.rs`, `item_persistence_service.rs` (ips). Upstream: `Emby.Server.Implementations/Library/UserDataManager.cs` (UDM), `Session/SessionManager.cs`, `MediaBrowser.Controller/Entities/BaseItem.cs`. J and F wrote the same five rows (played, favorite and rating; a stop at 300 s) with the same values, apart from the date form.

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| ItemId, UserId | udm:743-744 | UDM:66, 174 | match | user deletion cascades on both |
| CustomDataKey | `user_data_keys.rs:195-363`; udm:666-668 | UDM:58-75; `BaseItem.cs:1766-1779` and overrides | fix (gl-1610); edge fix (gl-1605) | upstream prepends the item's `ExternalId` for a channel source, which is every item with a `ChannelId`, Live TV programs included (`BaseItem.cs:288-299, 1768-1776`). Ferrofin's note at `crates/ferrofin-core/src/user_data_keys.rs:67-72` takes it for a channel-plugin case only, so a program's user data lacks that key. Edge: a negative index pads with the sign: `{:03}` gives `-01` where C#'s `"000"` gives `-001`, and disc and track `{:04}` likewise |
| AudioStreamIndex, SubtitleStreamIndex | `crates/ferrofin-traits/src/library.rs:2027-2041` via udm:813-829 | `SessionManager.cs:990, 1016-1055` | match | |
| IsFavorite, Rating, Likes | udm:1822-1852, 2007-2008 | UDM:130-153; `UserItemData.cs:96-118` | match | Likes derives from Rating (6.5 threshold) on both |
| PlayCount, PlaybackPositionTicks, Played | udm:1412-1520, 2123-2130 | UDM:455-529; `BaseItem.cs:2252-2302` | match | same thresholds |
| Played, PlayCount, PlaybackPositionTicks, LastPlayedDate (container unplay) | `user_data_manager.rs:1066-1084` clears the container's own rows | `MarkUnplayed` leaves them | owner? (gl-1064) | awaiting the owner on gl-1064 (`crates/ferrofin-api/src/handlers.rs:1393-1395`) |
| all (concurrent saves) | each save reads inside the write transaction, so both land | the later save overwrites the earlier one | owner? (documented) | `crates/ferrofin-api/src/handlers.rs:1390-1392` (P6) |
| LastPlayedDate | udm:1419-1456, 1516-1518, 2126 | `BaseItem.cs:2266, 2301`; `SessionManager.cs:852-855, 982-985` | fix (gl-1565, gl-1604) | datetime form. **Migration `0010_hermit_backfill_last_played.sql:14-16` runs on every adopted database** (not baselined: `crates/ferrofin-db/src/database.rs:1529-1557`). It sets `LastPlayedDate` to the adoption time on every `Played=1` row that has none, rows upstream writes on purpose (`MediaBrowser.Controller/Entities/Video.cs:388`, an NFO `watched` import). That invents play dates |
| RetentionDate | detach ips:2183; recover `user_data_retention_repository.rs:121`; migration 0042:99-101 | `ItemPersistenceService.cs:61, 129-132`; UDM:69 writes NULL on every save | fix (gl-1565, gl-1605) | datetime form. Ferrofin's update leg keeps an existing value (udm:719-720); upstream clears it. No practical effect: a live row holding one should not exist |

Also:
- **NFO user-data import is not ported** (gl-1606). `crates/ferrofin-providers/src/xbmc/base_parser.rs:309-316` says "User-data import is out of scope for the local parse; consume". Upstream saves `Played`, `PlayCount` and `LastPlayedDate` from an NFO when its configuration names a user (`MediaBrowser.XbmcMetadata/Parsers/BaseNfoParser.cs:308-370`), so Ferrofin writes no row where upstream does.
- **No upstream equivalent:**
  - the user-data retention reattach and recovery semantics (`docs/reviews/user-data-retention.md`; `ips:2237, 2247-2248`): owner? (documented), P6;
  - the scan rekeys and folds (`crates/ferrofin-traits/src/persistence.rs:1563-1566`, owner decisions D4 and D9b): recorded;
  - the 12.2 harmonize port's tie-break (`crates/ferrofin-core/src/adoption_repairs.rs:3421-3425`, `winner_order` at 3578-3588): owner? (documented), P6.

## Devices, DeviceOptions, ApiKeys, ActivityLogs

Ferrofin: `crates/ferrofin-core/src/session_manager.rs` (SM), `device_manager.rs`, `authorization_context.rs` (AC), `api_key_manager.rs`, `activity_manager.rs`, `apps/ferrofin-server/src/state.rs`. Upstream: `Emby.Server.Implementations/Session/SessionManager.cs`, `Jellyfin.Server.Implementations/Devices/DeviceManager.cs`, `Security/AuthorizationContext.cs`, `Security/AuthenticationManager.cs`, `Activity/ActivityManager.cs` and `Events/Consumers/**`.

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| Devices.Id, UserId, DeviceId, AppName | SM:3003-3011; `device_manager.rs:314-329` | `Device.cs:23-27`; `SessionManager.cs:1789-1810` | match | a re-login of one device replaces its row on both |
| Devices.AccessToken | SM:3002 | `Device.cs:29` | match | 32 lowercase hex in both runs |
| Devices.AppVersion, DeviceName (refresh) | AC:213-228 store the trimmed header | `AuthorizationContext.cs:158-177` store it untrimmed | match; edge fix (gl-1600) | a header with padding around a value; a whitespace-only one falls back to the stored value on both sides |
| Devices.IsActive | SM:3010 `is_active: true` | never set (`Device.cs:85`) | fix (gl-1600) | J holds 0, F holds 1, on the same logins |
| Devices.DateCreated | SM:3005 | `Device.cs:30` | fix (gl-1565, gl-1600) | datetime form. Ferrofin takes three separate `Utc::now()` readings for the three dates (SM:3005-3007); upstream copies one instant |
| Devices.DateModified | AC:253 moves it on every refresh | `Device.cs:31` sets the creation time, which never changes | fix (gl-1565, gl-1600) | datetime form |
| Devices.DateLastActivity | AC:231-234, after `num_minutes() > 3` | `AuthorizationContext.cs:180-184`, `TotalMinutes > 3` | fix (gl-1565, gl-1600) | datetime form. Whole minutes refresh only after 4. A cache hit skipping the refresh for up to 30 s is owner? (documented): AC:375-378, P6 |
| DeviceOptions.Id, DeviceId, CustomName | `device_manager.rs:546-549` | `DeviceManager.cs:84-88` | match | |
| ApiKeys.Id, Name, AccessToken | `api_key_manager.rs:100-104`; `handlers/api_key.rs:69-73` | `ApiKey.cs:21`; `AuthenticationManager.cs:31` | match; edge fix (gl-1601) | a whitespace-only `app` is stored, where upstream answers 400 |
| ApiKeys.DateCreated | `api_key_manager.rs:101-107` | `ApiKey.cs:22` | fix (gl-1565) | datetime form |
| ApiKeys.DateLastActivity | `api_key_manager.rs:104-105` seeds it with now | never set: `0001-01-01 00:00:00` in J's rows | fix (gl-1601) | the premise of the comment at `api_key_manager.rs:12-13` is false; gl-834 fixed only the wire value |
| ActivityLogs.Id, UserId, ItemId, LogSeverity, RowVersion | `activity_manager.rs:218-235` | `ActivityLog.cs:28-30, 115` | match | |
| ActivityLogs.Type, Name, ShortOverview, Overview | per event (below) | per consumer | fix (gl-1602, gl-1603, and the open gl-429, gl-430, gl-908) | |
| ActivityLogs.DateCreated | `activity_manager.rs:235` | `ActivityLog.cs:29` | fix (gl-1565) | datetime form |

**Activity log events.** The entries Ferrofin writes differ from upstream's consumers as follows. The first three were seen in J against F.
- **Failed logins** (gl-1602). Ferrofin writes `AuthenticationFailed` on every login error (`handlers/users.rs:350-365`). Upstream writes it only for a wrong password on an existing, enabled user: every other failure throws before the event (`UserManager.cs:620-707`). F wrote three rows J did not, for the disabled, locked and settings accounts.
- **Password changes** (gl-1602). `UserPasswordChanged` is missing after `POST /Users/New` with a password (`users.rs:699`; upstream `UserController.cs:527`) and after the startup wizard's user (`startup.rs:313`; upstream `StartupController.cs:164`). J wrote one row F did not.
- **Disconnects** (gl-1603). `SessionEnded` names the device the session started with. Ferrofin drops `DeviceOptionsUpdated` (`device_manager.rs:12`, "event wiring is out of scope"), so a live session never takes a custom name set after it started. Upstream renames it (`SessionManager.cs:160-172`). J: "audit-new has disconnected from Audit device". F: "… from Synthetic adoption". The same applies to `GET /Sessions`' `DeviceName`.
- **Edges** (gl-1602):
  - an empty remote endpoint gives a NULL `ShortOverview`, where upstream writes `"IP address: "` (`state.rs:2629-2631`);
  - a playback entry names the first non-empty artist, where upstream names `Artists[0]` (`playback_activity.rs:165`);
  - retention deletes `<` the cutoff, where upstream deletes `<=` (`activity_manager.rs:254`; `ActivityManager.cs:143`).
- **Already tracked as fixes:** English-only names and the `TaskFailed` text (gl-429, gl-908); plugin lifecycle and download entries (gl-430).
- **Recorded:** idle sessions ending with an entry (owner decision, gl-1267).
- **Ferrofin-only:** a `BackupRestoreFailed` entry (`apps/ferrofin-server/src/lib.rs:548-563`), owner? (P3).

## DisplayPreferences, ItemDisplayPreferences, CustomItemDisplayPreferences, HomeSection

Ferrofin: `crates/ferrofin-core/src/display_preferences_manager.rs` (DPM), `crates/ferrofin-api/src/handlers/display_preferences.rs` (DPH). Upstream: `Jellyfin.Server.Implementations/Users/DisplayPreferencesManager.cs`, `Jellyfin.Api/Controllers/DisplayPreferencesController.cs` (DPC). Both create a row on the first GET, with the same defaults. J and F matched apart from the skip lengths.

| Column | Ferrofin writer | Upstream writer | Verdict | Note |
|---|---|---|---|---|
| DisplayPreferences.Id, UserId, Client, ItemId, ShowSidebar, ShowBackdrop, ScrollDirection, IndexBy, ChromecastVersion, DashboardTheme, TvHome | DPM:100-121, 277-302; DPH:86-88, 295-327 | `DisplayPreferences.cs:20-30`; DPC:58-177 | match | same item-id derivation (gl-881); `IndexBy` per gl-880 |
| DisplayPreferences.SkipForwardLength, SkipBackwardLength | DPH:315-320 fall back to 30000 / 10000 | DPC:157-166 fall back to 15000 / 15000 | fix (gl-410) | owner-decided on gl-410 (2026-11-02): port the 15000 constant. The 10.11.8-pin comment at DPH:50-51 is the debt that decision removes. J stored 15000/15000, F 30000/10000 |
| DisplayPreferences.EnableNextVideoInfoOverlay | DPH:310-314 | DPC:152-155 | recorded | an unparsable value gives 1, where upstream fails the POST with a 500 (`handlers.rs:3326-3332`, "Jellyfin bug not ported") |
| ItemDisplayPreferences (all 10) | DPM:165-183, 310-325; DPH:386-389 | `ItemDisplayPreferences.cs:19-28`; DPC:210-215 | match | |
| CustomItemDisplayPreferences (all 6) | DPM:242-270 | DisplayPreferencesManager.cs:104-130 | match; recorded edge | the same keys are kept and dropped. A bare or malformed `homesection` key is recorded (`handlers.rs:3326`) |
| HomeSection.DisplayPreferencesId, Order, Type | DPH:348-359; `crates/ferrofin-db/src/database.rs:593-614` | DPC:179-190 | match | (gl-715) |
| HomeSection.Id | every save replaces the rows | upstream appends, and they pile up | recorded | gl-882; `handlers.rs:3333`, "Jellyfin bug not ported" |

**Item deletion keeps display preferences** (gl-1609). `delete_items` (`crates/ferrofin-core/src/item_persistence_service.rs:2314-2378`) leaves the deleted item's `ItemDisplayPreferences` and `CustomItemDisplayPreferences` rows. Upstream deletes them (`Jellyfin.Server.Implementations/Item/ItemPersistenceService.cs:142-143`). No foreign key holds them, so they remain as orphans Jellyfin would not have. A rekey leaving them on the old id is owner? (documented): `item_persistence_service.rs:4565-4567`, P6.

## SQL that upstream never runs

On these tables, beyond what the rows above cover:
- **Fresh databases only**, not run on adoption (`crates/ferrofin-db/src/database.rs:1529-1557` baselines it): migration `0007` uppercases GUIDs and rewrites `T`-form dates to three fractional digits (`crates/ferrofin-db/migrations/0007_jellyfin_10118_parity.sql:321-333, 410-419`).
- **An owner decision**, the adoption-realism milestone: `0044` deletes the rows of users that no longer exist (`0044_dangling_user_and_ancestor_cleanup.sql:14-19`).
- **Upgraded databases:** `0030_normalized_usernames.sql:13` and the `NormalizedUsername` repair (`crates/ferrofin-db/src/normalized_usernames.rs:91-102`), which match upstream's routine except for its `RowVersion` bump (gl-1607).
- **The same end state as upstream:**
  - explicit child deletes before a user's row (`crates/ferrofin-core/src/user_manager.rs:1424-1442`);
  - upserts where upstream reads then writes (`crates/ferrofin-core/src/device_manager.rs:546-549`, DPM:100-107);
  - the `HomeSection` replace (gl-882).

Two other writers change values on an adopted database: migration `0010` (gl-1604, a fix) and the hash upgrade on login (P4, for the owner).

## Follow-ups

Filed or extended by this audit. None of the issues filed here is in the milestone or carries a release target; the owner decides that. gl-1565, which got evidence, is a milestone phase targeted at v1.6.0.

| Issue | What |
|---|---|
| gl-1565 (evidence added) | the milestone's datetime investigation; its follow-up is one formatter that writes EF's trimmed form |
| gl-1600 | Device row bookkeeping: `IsActive`, `DateModified`, one creation instant, the refresh threshold, untrimmed name and version |
| gl-1601 | `ApiKeys.DateLastActivity` at .NET's minimum, and a whitespace-only `app` refused |
| gl-1602 | Activity entries: failed logins, password changes on create and in the wizard, the empty endpoint, `Artists[0]`, retention `<=` |
| gl-1603 | A device's new custom name reaches its live sessions (`DeviceOptionsUpdated`) |
| gl-1604 | Migration `0010` invents play dates on adopted databases |
| gl-1605 | `UserData` edges: `RetentionDate` cleared on a save, negative-index key padding |
| gl-1606 | NFO user-data import |
| gl-1607 | `RowVersion` on `Users`, `Permissions`, `Preferences` |
| gl-1608 | User rows: `CastReceiverId` at creation and the seeded admin's bitrate limit |
| gl-1190 (evidence added) | The startup wizard's case fold |
| gl-1609 | Item deletion removes display preferences |
| gl-1610 | Live TV programs' `ExternalId` user-data key |

Already open before this audit: gl-410 (skip lengths, owner-decided), gl-429, gl-430 and gl-908 (activity log), and gl-1064 (container unplay, awaiting the owner).

**For the owner (proposed accepted divergences):**
- **P1: the lockout counter.** Upstream counts the locking attempt twice, a Jellyfin bug.
  - Effect: at lockout, `Policy.InvalidLoginAttemptCount` reads one less in Ferrofin (2 instead of 3 with a limit of 2). Lockout itself behaves the same. No client shows the count (jellyfin-web shows only the lockout limit); it is visible in the policy JSON and the database.
  - Evidence: the fixture's locked user holds 3 with a limit of 2 (J:UM:680, 697).
  - Proposal: not ported, recorded with the other Jellyfin bugs not ported.
- **P2: `AccessSchedules` ids.** Upstream renumbers a user's schedule rows on every user save.
  - Effect: the ids appear in the policy's `AccessSchedules`, and upstream ignores them on save. No one notices: no client shows them.
  - Evidence: J:UM:231-235.
  - Proposal: accepted; the ids carry no meaning.
- **P3: the `BackupRestoreFailed` activity entry** (`apps/ferrofin-server/src/lib.rs:548-563`). It has no upstream event: Jellyfin aborts startup instead.
  - Effect: one Ferrofin-only row, which an admin sees in the activity log. After a return Jellyfin lists it like any other row, since `Type` is free text; that was not run.
  - Evidence: `apps/ferrofin-server/src/lib.rs:548-549`; upstream aborts the restore at startup (`Jellyfin.Server/Program.cs:205-212`).
  - Proposal: accepted, as the only trace a failed restore leaves.
- **P4: the legacy-hash upgrade on login.** Upstream means to store the upgraded hash but loses it to an `AsNoTracking` reload, a Jellyfin bug (`DefaultAuthenticationProvider.cs:82-87`, J:UM:569-571).
  - Effect: Ferrofin stores the stronger PBKDF2 hash once, and Jellyfin reads it back fine (run R logged in on a hash Ferrofin wrote). Upstream re-migrates in memory on every login. No user notices; only the stored hash differs.
  - Proposal: not ported, so Ferrofin keeps storing it, and the comment at UM:1556-1558 is corrected. gl-1607 leaves this path's `RowVersion` bump to the same decision.
- **P5: the bootstrap user's name.** Ferrofin seeds `admin` (or `FERROFIN_ADMIN_USER`); upstream names it after the host user (J:UM:756).
  - Effect: only a fresh install's first user, which its admin sees at the first sign-in. The code flags it for confirmation (`apps/ferrofin-server/src/config.rs:52`).
  - Proposal: the owner confirms `admin`, or Ferrofin ports the host user name.
- **P6: differences the code documents, with no owner decision yet.** Each is deliberate Ferrofin behaviour with no effect on what Jellyfin reads after a return.
  - concurrent user-data saves both landing (`crates/ferrofin-api/src/handlers.rs:1390-1392`);
  - the harmonize tie-break (`crates/ferrofin-core/src/adoption_repairs.rs:3421-3425`);
  - the user-data retention reattach semantics (`docs/reviews/user-data-retention.md`);
  - a device refresh delayed up to 30 s on a cache hit (AC:375-378);
  - a rekey leaving display preferences on the old id (`crates/ferrofin-core/src/item_persistence_service.rs:4565-4567`).

  Proposal: accept them as documented.
