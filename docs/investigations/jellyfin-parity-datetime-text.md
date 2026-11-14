# Stored DateTime text: Ferrofin against Jellyfin

Investigation gl-1565, a phase of the backwards-migration milestone. Run on `release/v1.6.0` @ `978bea505` on
2026-11-10. The stored stamps quoted below are UTC, which had already reached 2026-11-11. Compared against Jellyfin
12.4 (`UPSTREAM_TAG` `v12.4`) and the EF Core SQLite provider it pins.

## Summary

Jellyfin writes every `DateTime` column with the trailing zeros of its seven-digit tick fraction removed, and without
the `.` when the fraction is zero. A whole second is stored as `2020-01-01 00:00:00`. Ferrofin writes seven digits
always (`2020-01-01 00:00:00.0000000`), and writes RFC 3339 for a by-name row created on demand
(`2026-11-11T00:54:23.994092718+00:00`).

SQLite compares TEXT byte by byte, so one instant in two forms compares unequal, and the shorter text is the smaller.
On a database Jellyfin wrote, `MinPremiereDate` therefore drops an item that premiered exactly at the bound. After an
export, Jellyfin's `<=` comparisons do the same on rows Ferrofin wrote.

The verdict is to match upstream (the milestone's default, in its decision D8), filed as gl-1618.

## The writers

Upstream paths are relative to the Jellyfin repository at `v12.4` (its database projects live under `src/`), and to
dotnet/efcore at `v10.0.14`.

- **Jellyfin.**
  - Jellyfin pins Microsoft.Data.Sqlite and Microsoft.EntityFrameworkCore.Sqlite 10.0.14
    (`Directory.Packages.props:35,38`).
  - In that release, `SqliteValueBinder.Bind` (efcore `src/Microsoft.Data.Sqlite.Core/SqliteValueBinder.cs:42`; the
    `DateTime` branch is `:81-94`) writes the text at `:91`:
    `dateTime.ToString(@"yyyy\-MM\-dd HH\:mm\:ss.FFFFFFF", CultureInfo.InvariantCulture)`. The `F` specifier omits
    trailing zeros, and omits the `.` before it when nothing is left.
  - The only converter on `DateTime` columns is applied by `SetDefaultDateTimeKind(DateTimeKind.Utc)`
    (`src/Jellyfin.Database/Jellyfin.Database.Providers.Sqlite/SqliteDatabaseProvider.cs:144`, defined in
    `ModelBuilderExtensions.cs:42-46` beside it). That converter is `DateTimeKindValueConverter`
    (`ValueConverters/DateTimeKindValueConverter.cs:16-19`). It converts a value to UTC on the way in and sets its
    kind on the way out, and does no formatting. So the binder's format is what reaches the database.
- **Ferrofin.**
  - `ferrofin_db::store::datetime_to_db` (`crates/ferrofin-db/src/store.rs:40-47`) formats `"{}.{:07}"`: seven
    digits, zeros kept. Its module doc (`:4-5`) states that Jellyfin writes seven digits, which holds only when the
    seventh digit is not zero.
  - `ensure_by_name_item` (`crates/ferrofin-core/src/item_persistence_service.rs:2963`) binds `chrono::Utc::now()`
    directly. sqlx-sqlite 0.9.0 encodes a `DateTime<Tz>` with `to_rfc3339_opts(SecondsFormat::AutoSi, false)`
    (`sqlx-sqlite/src/types/chrono.rs:64-69`). That gives a `T`, the smallest exact width of 0, 3, 6 or 9 fractional
    digits, and `+00:00`. Nine digits on this host (nanosecond clock); six where the clock counts microseconds.
  - Three migrations write dates in SQL:
    - `0010` and `0042` append `'0000'` to `strftime('%f')`, which is the seven-digit form again.
    - `0007_jellyfin_10118_parity.sql:402-421` rewrote RFC 3339 values in 20 date columns with
      `strftime('%Y-%m-%d %H:%M:%f', …)`. That gives three digits with zeros kept (`.SSS`), a fourth form that a
      database migrated through `0007` can still hold.

## Observed

### Jellyfin 12.4

The database was a copy of the synthetic `jellyfin-12.4-fresh` adoption fixture: the official image's own install and
scan. The 21 `DateTime` columns are the TEXT date columns of the schema Ferrofin pins
(`crates/ferrofin-db/tests/data/jellyfin-12.2-schema.sql`; 12.4 adopts as the 12.2 generation, and the 12.0 and
10.11.8 schemas have the same set). This statement tallies their values by the number of fractional digits:

```sql
WITH d(c, v) AS (
      SELECT 'ActivityLogs.DateCreated' AS c, "DateCreated" AS v FROM "ActivityLogs"
      UNION ALL SELECT 'ApiKeys.DateCreated', "DateCreated" FROM "ApiKeys"
      UNION ALL SELECT 'ApiKeys.DateLastActivity', "DateLastActivity" FROM "ApiKeys"
      UNION ALL SELECT 'BaseItemImageInfos.DateModified', "DateModified" FROM "BaseItemImageInfos"
      UNION ALL SELECT 'BaseItems.DateCreated', "DateCreated" FROM "BaseItems"
      UNION ALL SELECT 'BaseItems.DateLastMediaAdded', "DateLastMediaAdded" FROM "BaseItems"
      UNION ALL SELECT 'BaseItems.DateLastRefreshed', "DateLastRefreshed" FROM "BaseItems"
      UNION ALL SELECT 'BaseItems.DateLastSaved', "DateLastSaved" FROM "BaseItems"
      UNION ALL SELECT 'BaseItems.DateModified', "DateModified" FROM "BaseItems"
      UNION ALL SELECT 'BaseItems.EndDate', "EndDate" FROM "BaseItems"
      UNION ALL SELECT 'BaseItems.PremiereDate', "PremiereDate" FROM "BaseItems"
      UNION ALL SELECT 'BaseItems.StartDate', "StartDate" FROM "BaseItems"
      UNION ALL SELECT 'Chapters.ImageDateModified', "ImageDateModified" FROM "Chapters"
      UNION ALL SELECT 'Devices.DateCreated', "DateCreated" FROM "Devices"
      UNION ALL SELECT 'Devices.DateModified', "DateModified" FROM "Devices"
      UNION ALL SELECT 'Devices.DateLastActivity', "DateLastActivity" FROM "Devices"
      UNION ALL SELECT 'ImageInfos.LastModified', "LastModified" FROM "ImageInfos"
      UNION ALL SELECT 'UserData.LastPlayedDate', "LastPlayedDate" FROM "UserData"
      UNION ALL SELECT 'UserData.RetentionDate', "RetentionDate" FROM "UserData"
      UNION ALL SELECT 'Users.LastActivityDate', "LastActivityDate" FROM "Users"
      UNION ALL SELECT 'Users.LastLoginDate', "LastLoginDate" FROM "Users")
SELECT CASE WHEN instr(v, 'T') > 0 THEN 'rfc3339' WHEN instr(v, '.') = 0 THEN 'none' ELSE length(v) - 20 END AS digits,
       count(*) AS "values", count(DISTINCT c) AS columns,
       sum(instr(v, '.') > 0 AND v LIKE '%0' AND instr(v, 'T') = 0) AS trailing_zero
FROM d WHERE v IS NOT NULL GROUP BY 1 ORDER BY 1;
```

```text
digits | values | columns | trailing_zero
5      |      2 |       2 | 0
6      |     38 |       7 | 0
7      |    393 |      15 | 0
none   |     36 |       4 | 0
```

These are 469 values in the 18 columns that hold one, and no fraction ends in a zero. Examples:
- `BaseItems.PremiereDate`: `2020-01-01 00:00:00` (all 14 values have no fraction).
- `BaseItems.DateCreated`: `2026-11-04 07:54:06`, `2026-11-04 07:46:26.447945`.
- `UserData.LastPlayedDate`: `2026-01-01 12:00:00`.
- `ApiKeys.DateLastActivity`: `0001-01-01 00:00:00`.

### Ferrofin

Steps on a scratch server built at `978bea505`:

1. Write one movie, `movies/Film (2020)/Film (2020).mkv`, as a two-second test pattern
   (`ffmpeg -f lavfi -i testsrc=duration=2:size=320x240:rate=10 -c:v libx264 -pix_fmt yuv420p "Film (2020).mkv"`;
   any video ffprobe can read will do), with this `movie.nfo` beside it:

   ```xml
   <?xml version="1.0" encoding="utf-8" standalone="yes"?>
   <movie>
     <title>Film</title>
     <year>2020</year>
     <premiered>2020-01-01</premiered>
     <genre>Drama</genre>
     <studio>Example Studio</studio>
   </movie>
   ```

2. Start the server on an empty data directory with `FERROFIN_ADMIN_PASSWORD=pw ferrofin-server --data-dir <dir>
   --bind 127.0.0.1 --port 18981`. Sign in with `POST /Users/AuthenticateByName` and the body
   `{"Username": "admin", "Pw": "pw"}`, and send the returned token on every later request
   (`Authorization: MediaBrowser Client="probe", Device="probe", DeviceId="probe", Version="1", Token="<token>"`;
   the sign-in carries the same header without `Token`).
3. Add the library with `POST /Library/VirtualFolders?name=Movies&collectionType=movies&paths=<movies>&refreshLibrary=true`
   and the body
   `{"LibraryOptions": {"PathInfos": [{"Path": "<movies>"}], "MetadataSavers": [], "LocalMetadataReaderOrder": ["Nfo"], "TypeOptions": [{"Type": "Movie", "MetadataFetchers": [], "ImageFetchers": []}]}}`.
   Then wait until `GET /Items?Recursive=true&IncludeItemTypes=Movie` lists the movie.
4. Stop the server and run the statement above on `<dir>/ferrofin.db`:

   ```text
   digits | values | columns | trailing_zero
   7      |     30 |      11 | 7
   ```

   Every value the scan wrote in the 21 columns has seven digits, and 7 of the 30 keep trailing zeros. Examples:
   `BaseItems.PremiereDate` is `2020-01-01 00:00:00.0000000`, and a `DateModified` is `2026-11-11 00:53:27.3326680`.
5. Start it again and request `GET /Genres/Comedy`, a genre no row holds (the NFO's genre is `Drama`). The new row
   stores `DateCreated` and `DateModified` as `2026-11-11T00:54:23.994092718+00:00`.

The observation that opened this phase reproduces: `PremiereDate` is `2020-01-01 00:00:00.0000000` in Ferrofin and
`2020-01-01 00:00:00` in Jellyfin.

## Readers and comparisons

Reading is unaffected in both directions:
- sqlx decodes all of these forms (it tries RFC 3339, then `%F %T%.f`), which is why adopted databases work.
- The stored text never reaches a response. The DTO formats the parsed instant (`2020-01-01T00:00:00.0000000Z`).
- gl-1565 records that Jellyfin read Ferrofin's seven-digit text without error in the 2026-11-09 round trip, and that
  its first scan rewrote the rows it saved.
- Whether Jellyfin reads the RFC 3339 form as the same instant was not checked.

Comparisons are affected when a stored value and a bound value name the same instant in different forms. The shorter
text is a prefix of the longer one and sorts first:
- With Jellyfin's trimmed text stored and Ferrofin's padded text bound, `col >= bound` and `col = bound` are false.
  `col <= bound` is true, but only by accident.
- With Ferrofin's padded text stored and Jellyfin's trimmed text bound, as after an export, `col <= bound` and
  `col = bound` are false.

Between different instants the order is right. A fraction without trailing zeros compares as a decimal, since its
digits stay aligned after the `.`, and padding only appends zeros. So only equal instants misorder. The `T` form is
the exception: it sorts after every space-form value of the same date.

Ferrofin's comparisons bind `datetime_to_db` text:
- **Range filters:**
  - `translate_query.rs`:
    - `MinPremiereDate` and `MaxPremiereDate` (`:1597-1602`);
    - `MinStartDate`, `MaxStartDate`, `MinEndDate` and `MaxEndDate` (`:1581-1594`);
    - `MinDateCreated` (`:1605`) and `MinDateLastSaved` (`:1617`);
    - `IsAiring` and `IsUnaired` (`:1622-1629`; `HasAired` sets the `EndDate` bounds above, at `:1575-1579`);
    - the latest-items cutoff (`:570`).
  - The activity log's `MinDate` and `MaxDate` (`activity_manager.rs:146,150`).
  - The plugin offered-items window (`plugin_offered_item_repository.rs:94`).
  - Next Up's cutoff (`next_up_service.rs:338`, used at `:1031`).
- **Equality.** An adopted row whose stored text has fewer than seven digits counts as changed in each of these, and
  is rewritten with a new `DateLastSaved`:
  - `update_file_facts`, `"DateModified" IS NOT ?5` (`item_persistence_service.rs:3757`, bound from
    `opt_datetime_to_db` at `:3735` and `:3764`). This runs on the scan's per-item path, and a file's modification
    time often ends in a zero tick, so it reaches many rows. Its `DateCreated` case (`:3747-3750`) compares
    `julianday` values with a tolerance and is unaffected.
  - The media-added stamp's change check, `"DateLastMediaAdded" IS NOT ?2` (`:3867`, bound at `:3870`).
- **Older-than guards and purges:**
  - `user_activity_repository.rs:25`;
  - `user_manager.rs:1266`;
  - `user_data_retention_repository.rs:176`;
  - `item_persistence_service.rs:6753`;
  - the activity-log purge (`activity_manager.rs:254`).
- **Ferrofin's own tables.** They hold only Ferrofin's text, so they compare consistently today.
  - Compared against these binds:
    - the Live TV guide filters on `FerrofinLiveTvPrograms` `StartDate` and `EndDate`
      (`crates/ferrofin-livetv/src/manager.rs:7039-7094`). The programs are written at `:834-835` (XMLTV) and
      `:2583-2584` (Schedules Direct);
    - the user-data retention snapshots' `RetentionDate` (`user_data_retention_repository.rs:168`).
  - Only stored in this form:
    - `FerrofinLiveTvChannels.DateCreated` (`manager.rs:2164`), sorted as text in memory (`projection.rs:246`);
    - the DVR timers and recordings (`dvr_repository.rs:76-77,276-280`);
    - `FerrofinPluginOfferedItems.OfferedAt` (`plugin_offered_item_repository.rs:58,91,99`).
- **Already handles both forms:** the adoption repair `CONFLICTING_GROUPS` (`adoption_repairs.rs:3389-3411`) pads each
  `LastPlayedDate` to seven digits before comparing, and documents the two forms.

Reproduced on the scratch server. With the server stopped, the movie's `PremiereDate` was set to the text Jellyfin
stores, as an adopted row keeps it until Ferrofin rewrites the row:

```sql
UPDATE "BaseItems" SET "PremiereDate" = '2020-01-01 00:00:00' WHERE "Name" = 'Film';
```

After a restart, `GET /Items?Recursive=true&IncludeItemTypes=Movie&Fields=PremiereDate` with each bound returned:

| Bound | Ferrofin's own row | Jellyfin's text |
|---|---|---|
| `&MinPremiereDate=2020-01-01T00:00:00.0000000Z` | `['Film']` | `[]` |
| `&MaxPremiereDate=2020-01-01T00:00:00.0000000Z` | `['Film']` | `['Film']` |

Jellyfin binds the trimmed form, so it keeps the movie in both. The `Min` and `Max` filters are mostly given dates,
and a premiere date is midnight almost always, so an equal instant at the bound is the common case, not a corner.

## Verdict

Fix to match upstream, filed as gl-1618:
1. `datetime_to_db` writes the binder's form.
2. The raw `chrono` bind in `ensure_by_name_item` goes through it.
3. A new SQLite migration rewrites, into that form, the stored values Ferrofin wrote in the 21 columns: seven
   digits, 0007's three digits, and the RFC 3339 form with any of its widths.

No accepted divergence is proposed, because Ferrofin's form has no advantage to keep. The Ferrofin tables compared
against these binds (the guide programs and the retention snapshots) move with the change, by a migration or by a
rebuild such as a guide refresh, or their comparisons split. The tables that only store the form can move with them.
`CONFLICTING_GROUPS` keeps working either way.

The 21 `DateTime` columns:
- `ActivityLogs.DateCreated`;
- `ApiKeys.DateCreated` and `DateLastActivity`;
- `BaseItemImageInfos.DateModified`;
- `BaseItems.DateCreated`, `DateLastMediaAdded`, `DateLastRefreshed`, `DateLastSaved`, `DateModified`, `EndDate`,
  `PremiereDate` and `StartDate`;
- `Chapters.ImageDateModified`;
- `Devices.DateCreated`, `DateModified` and `DateLastActivity`;
- `ImageInfos.LastModified`;
- `UserData.LastPlayedDate` and `RetentionDate`;
- `Users.LastActivityDate` and `LastLoginDate`.

The PostgreSQL binding is outside this investigation.
