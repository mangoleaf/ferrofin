# Adoption smoke test

Ferrofin supports in-place adoption of Jellyfin **10.11.8, 10.11.9, 10.11.10, 10.11.11,
12.0.0, 12.1.0 and 12.2.0** databases. Adoption is one-way; returning to Jellyfin requires
restoring a backup. Follow the
[migration procedure](../../docs/INSTALL.md#migrate-an-existing-jellyfin-installation) for a
real installation.

## Supported and tested versions

The status every release's gates session found, per version and upgrade route, is published with
that release in the root README's [Results](../../README.md#adopting-an-existing-jellyfin-database).
The record below is the per-version history.

The 10.11 and 12.0–12.1 fixture paths below **passed** the live suite on **2026-09-16
(US/Mountain)**, against fixtures derived from a real library. The suite no longer uses real
libraries: since 2026-10-14 every fixture is [synthetic](#the-fixtures-are-synthetic). All ten
paths, 12.2 included, **passed** both `run.sh` and `synthetic.py run` on synthetic fixtures on
**2026-10-17**; the synthetic results follow the table.

Each test booted Ferrofin on a fresh copy of a Jellyfin database. The 10.11.8 fixture was the
original snapshot; the others were produced by booting the corresponding Jellyfin releases on
copies, as shown below.

| Source Jellyfin version | Fixture upgrade path before adoption | EF migration IDs | Generation logged by Ferrofin | Live result |
|---|---|---:|---|---|
| 10.11.8 | Original 10.11.8 snapshot | 68 | `10.11.8` | PASS |
| 10.11.9 | 10.11.8 → 10.11.9 | 68 | `10.11.8` | PASS |
| 10.11.10 | 10.11.8 → 10.11.10 | 71 | `10.11.11` | PASS |
| 10.11.11 | 10.11.8 → 10.11.11 | 71 | `10.11.11` | PASS |
| 12.0.0 | 10.11.8 → 12.0.0 | 102 | `12.0.0` | PASS |
| 12.1.0 | 10.11.8 → 12.1.0 | 104 | `12.1.0` | PASS |
| 12.1.0 | 10.11.8 → 12.0.0 → 12.1.0 | 105 | `12.1.0` | PASS |
| 12.2.0 | 10.11.8 → 12.2.0 | 107 | `12.2.0` | PASS (synthetic) |
| 12.2.0 | 10.11.8 → 12.0.0 → 12.2.0 | 108 | `12.2.0` | PASS (synthetic) |
| 12.2.0 | 10.11.8 → 12.0.0 → 12.1.0 → 12.2.0 | 109 | `12.2.0` | PASS (synthetic) |
| 12.2.0 | Fresh 12.2 install (not upgraded) | 105 | `12.2.0` | pending — Adoption workflow |

The gate in `crates/ferrofin-db/src/database.rs` matches migration **ID sets**, not just
their counts or a version string. Releases sharing a set share the logged generation.
Both 12.1 paths are accepted because the older rating-level migration ID is optional;
both 12.2 paths because the 12.0-era and 12.1-era optional ids are carried forward. A fresh
12.2 install carries the smallest set (105 ids, without the retired 10.11-era routines) and
the fresh-install schema (no `IX_CustomItemDisplayPreferences_UserId`), yet still adopts as
`12.2.0`; `0041` gives it that index. Unknown or incomplete histories are refused; the listed
versions do not imply support for other 10.11.x or 12.x releases. Other upgrade routes were not
separately exercised by the live run.

The tested server image for 10.11–12.1 was `ferrofin:adopt121`, supplied as built from
server commit `1732f97c`, with local image ID
`sha256:0b29af8f135a83d196329a0ada9f41f0ecd7d851e98dde9092d4dd01766ca42d`.
The run used the corrected harness that reports SQLite query failures and detects
alternate-version promotions even when `repaired` is zero. All seven fixtures passed
with exit status 0. Later server changes require rerunning this matrix.

**Correction (2026-10-17):** in that run, and in the `run.sh` runs recorded under
[Metadata gate validation](#metadata-gate-validation) and
[Watch-history gate validation](#watch-history-gate-validation), the repeated-repair half of
check 4 below could not fail. `run.sh` selected the second boot's log with a UTC time that
Docker read as local time, so on a host west of UTC the log was always empty. Those PASS
results do not cover that half; the second boot's probe summaries were still compared. `run.sh`
now reads the log from the restarted container's own start time and fails a log with no
server start. The synthetic preservation matrix (`synthetic.py run`) slices restart logs by
line offset and was not affected.

### 2026-10-14 synthetic run

All ten paths were built from synthetic data by `build-fixtures.sh` and run with image
`ferrofin:adopt-synthetic-27a22ea5` (server code of `release/v1.4.0` at `27a22ea5`), using the
harness as committed. Every fixture had its release's EF migration-ID count: 68, 68, 71, 71, 102,
104, 105, 107 (12.2 from 10.11), 108 (12.2 from 12.0) and 109 (12.2 from 12.1). Jellyfin 12.2
gave identical smoke answers on the 10.11 and 12.0 lineages.

| Check | 10.11.8–10.11.11 (4 paths) | 12.0, 12.1 ×2, 12.2 ×3 (6 paths) |
|---|---|---|
| `synthetic.py run`, all four stages | PASS | PASS |
| `run.sh`: generation, migrations, no `ERROR` | PASS | PASS |
| `run.sh`: integrity and foreign keys | PASS | PASS |
| `run.sh`: repair-free second boot (log holds exactly one server start) | PASS | PASS |
| `run.sh`: metadata and watch history after adoption, restart and scan | PASS | PASS |
| `run.sh`: smoke parity with Jellyfin 12.2 | **FAIL**: playlists + persons | **FAIL**: playlists |

The smoke failures are two Ferrofin bugs, neither new on `release/v1.4.0`: both reproduce on
builds from September onward.

- **Playlists (all ten paths):** an administrator sees another user's two private playlists in
  `/Items?IncludeItemTypes=Playlist` and gets a Playlists view; Jellyfin 12.2 shows neither.
- **Persons (10.11 sources only):** Ferrofin's scan never records album artists as people, so
  `/Persons` returns 0 where Jellyfin 12.2 returns 2. Every 12.x fixture already holds the
  artist `Peoples` rows Jellyfin 12.x wrote, and Ferrofin lists those correctly.

### 2026-10-17 synthetic run

A fresh fixture directory on a local disk was built by `build-fixtures.sh` (every EF
migration-ID count as above), and both runners used it with image
`ferrofin:adopt-synthetic-1906c8f6`: server code of `release/v1.4.0` at `1906c8f6`, local image ID
`sha256:7ef754ae19910972dbf879b8c3e46ab6a2a9884deb7039281831d8ef797bffd1`. `run.sh` and
`synthetic.py run` ran at the same time against the same directory, `run.sh` under `work/run/`.

- `run.sh`: **PASS** on all ten paths, exit status 0. This covers every check, including
  smoke parity with Jellyfin 12.2, the 10.11 paths' post-scan `/Persons` comparison, and both
  halves of check 4 (the second-boot log must hold one server start). Neither smoke failure
  recorded on 2026-10-14 reproduces at `1906c8f6`.
- `synthetic.py run`: **PASS** on all ten paths at all four stages (40 of 40).

Two earlier builds on the same host, under a load average near 27, stopped in the 10.11.8 seed
with "the linked … has no live member"; each was rebuilt into a new directory. That seeding
failure is tracked separately.

### 2026-10-19 table digests (check 7)

Fixtures built fresh by `build-fixtures.sh` (recipe at this commit). Image `ferrofin:bench`,
id `4961433c3034`, built 2026-10-19 18:26 MDT from `release/v1.4.0` (its commit was not
recorded; the harness-only change here does not depend on it). `run.sh`: **PASS** on all ten
paths. `synthetic.py run`: **PASS** on all ten paths at all four stages (40 of 40). The first
runs failed on three things the normaliser now names, none a Ferrofin data loss: a re-login
replaces a `Devices` row, a client re-announces itself, and the rehome repair re-keys a local
alternate version (as Jellyfin 12.2 does), moving its MediaStreamInfos and ItemValuesMap rows
to the new item id. The fixtures hold rows in only some of the listed tables (Chapters,
TrickplayInfos, KeyframeData, MediaSegments and the display-preference tables are empty
until the fixtures populate them).

### 2026-10-20 fresh-install 12.2 row (gl-632)

A fresh-install Jellyfin 12.2 database (a first run, not upgraded from 10.11) was added as the
eleventh live `run.sh` row (`jellyfin-12.2-fresh`), seeded from scratch through the 12.2 API and
compared against its own Jellyfin 12.2 oracle (`smoke-jellyfin-12.2-fresh.txt`), since a clean
install is a distinct library from the seeded upgrade routes. It carries the fresh-install EF id
set (105) and schema and adopts as `12.2.0`. The offline gates all pass — `bats gates/adoption/tests`,
`python3 -m unittest discover -s gates/adoption/tests` (170 tests) and ShellCheck — but the live
`run.sh`/`build-fixtures.sh` could not run on the author's host (its system `containerd` was
down). The live result above is therefore recorded as **pending**: the `Adoption` workflow
exercises this row on the next push to `release/v1.4.0`, and `release-gate` blocks the tag until
it is green. Update the table row and this note once that run lands.

### 2026-10-20 messy rows, first slice (gl-629)

Fixtures built fresh by `build-fixtures.sh` into a new directory (the build had stopped at the
fresh-install fixture on every run until gl-701, so this is also the first complete build at that
recipe: eleven fixtures, three oracle smoke files, the schema pin), then `messy.py build` for
the two messy copies. The oracle was Jellyfin 12.2 image id `sha256:357724bf0ae2…` (pulled by the
build and recorded in each `messy-oracle.json`); its all-shapes run matched its single-shape runs
on both lineages. Ferrofin image `ferrofin:adopt-gl663b` (built 2026-10-19 21:00 MDT from the
gl-663 branch, which includes migration 0044; it was not rebuilt from the current
`release/v1.4.0` head, so the release run must use its own candidate image). `run.sh --only
jellyfin-10.11.11-messy`: **PASS** (the shapes end as the oracle and the written divergences
say, the table digests hold, the second boot is quiet). `run.sh --only jellyfin-12.2-messy`:
**FAIL, as expected**: Ferrofin refuses the database (`database has 1 foreign-key violation(s)
after migrating (BaseItems → BaseItems: 1)`) because of the dangling-owner extra, which Jellyfin
12.2 boots; gl-707 tracks it, and the row passes only once that is fixed. The run ends at once
with every reason named, not after the readiness deadlines. The other two shapes on that lineage
were checked on a scratch copy without the extra (`messy.py check` exit 0). The eleven clean rows
and `synthetic.py run` were not re-run for this change, which touches none of them.

### 2026-10-20 scale row (gl-631)

Every fixture rebuilt by `build-fixtures.sh` into a new directory, so the populated tables of
gl-628 are in. It was built before the missing-count and duplicate-page guards went into
`preservation.py`, so `synthetic.py run` on it would ask for a rebuild; the scale row does not read
the recipe. The build also ran
`scale.py build`: `jellyfin-10.11.11-scale`, a 253 MiB database of 39,933 cloned items on top of the
fixture's own 67 (the clones carry 139,770 chapter rows, 97,837 stream rows, 55,908 people links,
27,954 trickplay rows, 27,954 keyframe rows and 7,986 UserData rows). Ferrofin image
`ferrofin:adopt-gl631`, built from this change on top of `release/v1.4.0` (migration 0044, not
0045); the release run needs its own candidate image. `run.sh --only jellyfin-10.11.11-scale`
**PASSED** three times and recorded the numbers each time. Runs 1 and 2 predate the review fixes
to the harness (run 1 also the anonymous-memory figure); run 3 is on the final harness:

| Number | Run 1 | Run 2 | Run 3 (final harness) |
|---|---|---|---|
| migration time (server start to migrations applied) | 2.9 s | 2.9 s | 5.4 s |
| peak memory (container cgroup, page cache included; the whole first boot including the boot repairs) | 1.4 GiB | 1.5 GiB | 1.3 GiB |
| anonymous memory (largest seen by the sampler) | not yet recorded | 243.5 MiB | 245.7 MiB |
| disk high-water (data directory, WAL and snapshots included) | 1,454 MiB, 5.7x the database | 1,454 MiB | 1,454 MiB |
| largest WAL seen | 140 MiB | 140.1 MiB | 161.8 MiB |
| `foreign_key_check` on the adopted file | 0.2 s | 0.2 s | 0.2 s |

Most of the cgroup peak is page cache (about 245 MiB of 1.3 to 1.5 GiB is anonymous), so the cgroup
figure is not a measure of resident memory and should not become a memory threshold. Three runs on
a shared, loaded host with the database on a local NVMe disk: an indication, not a baseline. Disk
is the same to the MiB every time and the `foreign_key_check` steady; memory and the WAL peak move
a little; the migration time doubled on the third run, which was not investigated (other builds
were running on the host throughout). The measured high-water of the whole data directory (5.7x the database)
is only about 0.3 of the database below the free-disk floor (`FREE_MULTIPLE` = 6 times the
database, plus the rest of the fixture); raise the multiple when a threshold is proposed.
The two messy rows were run again with the same image because `wait_up` now takes a deadline:
`jellyfin-10.11.11-messy` **PASS**, `jellyfin-12.2-messy` fails at once for the reason in the
messy record above (this image has no 0045). The eleven clean rows and `synthetic.py run` were not
re-run: they wait with `wait_ready`, which did not change (`wait_up`, which the messy and scale
rows use, now takes a deadline whose default equals the old 300 polls of 2 s), and the one thing
they share with this change is `preservation.py`'s snapshot, which pages through every item and is
covered by its unit tests.

### 2026-10-20 messy rows, second slice (gl-630)

The messy copies were rebuilt with all nine shapes (`messy.py build`: Jellyfin 12.2 image id
`sha256:357724bf0ae2…`, ten boots per lineage, each shape alone and all together, the two agreeing).
They sit in the fixture directory built for gl-631, whose recipe is one edit ahead of this branch's
(`run.sh` does not read the recipe; the clean fixtures are unchanged). Ferrofin image
`ferrofin:adopt-gl630`, built from this change on top of `release/v1.4.0` (migrations through 0045).
`run.sh --only jellyfin-12.2-messy`: **PASS**, with the divergences above written. `run.sh --only
jellyfin-10.11.11-messy`: **FAIL, as expected**, on `gone_file_userdata` alone (the item is kept,
Jellyfin 12.2 removes it: the open decision gl-521); every other shape, including both case-duplicate
pairs, the GUID spellings, the virtual season and the non-ASCII names, ends exactly as Jellyfin 12.2
left it. Getting there found five faults in the seeds themselves, each now a test: a clone with no
file (removed by the upgrade before its shape showed anything), a shape that rewrote another's
original (the combined run differed from the single runs), a person clone that shared its original's
metadata path (the duplicate-path repair then stood in for the merge), people counted by id although
Ferrofin re-keys them, and a table-digest baseline that read a legitimate merge as a lost row. The
eleven clean rows and `synthetic.py run` were not re-run: nothing they use changed.

### 2026-10-21 the report (gl-633)

`report.html` written by both entry points, on synthetic fixtures with Ferrofin image
`ferrofin:adopt-gl630` (built from `release/v1.4.0`, migration 0045; the release run needs its own
candidate image). `run.sh` ran against existing synthetic fixtures (it does not compare them to the
recipe): `--only 12.2.0` adopted the five 12.2.0 rows (`from-10`, `from-12`, `from-121`, `fresh`
and the `messy` copy with its three original shapes), all **PASS**, 78 checks recorded, the page
written in 99 s with the messy shapes table and every stage's checks; `--only
jellyfin-10.11.11-scale` **PASSED** and its page carries the same measurements as the PASS line
(migration 4.7 s, peak memory 950.1 MiB, anonymous 234.1 MiB, disk high-water 1.4 GiB, WAL 161.8
MiB, `foreign_key_check` 0.3 s). The pages open from disk and the viewer's Adoption tab lists them
(`gates/bench/report.py --serve --adoption-dir DIR/work`). `preservation.py` is part of the synthetic
recipe, so the report change needs the fixtures built again for `synthetic.py run`: the five
fixtures it needs (`jellyfin-10.11.8`, `12.0`, `12.1-from-12`, `12.2-from-121`, `12.2-from-10`) were
built into a new directory and `synthetic.py run --only jellyfin-12.2-from-10` **PASSED** all four
stages, its page listing 35 checks under the same stage names as `run.sh`'s. The other paths of the
matrix, and the messy shapes added to the messy fixtures later, were not run again for this change.

## What the live suite checks

1. The boot log names the expected generation, reports completed database migrations and
   logs no `ERROR`.
2. The read-only probes in `smoke.sh` produce the same normalised summaries as **Jellyfin
   12.2** on the same library. IDs and image byte counts are normalised; server-info,
   task, plugin, device, virtual-folder, activity-log and session lines are excluded.
   A 10.11 source has no music-artist credits until its first 12.x or Ferrofin scan
   (10.11 never creates them; 12.x creates them on a metadata refresh, `AudioFileProber`
   and `AlbumMetadataService.SetPeople`). So a 10.11 path compares `/Persons` only after
   the scan in check 5, where the whole smoke is compared with the oracle again. This
   relies on the oracle and the 12.x fixtures carrying their release's own scan, which
   `synthetic.py` guarantees: it scans every fixture it derives before recording it.
3. `PRAGMA integrity_check` and `PRAGMA foreign_key_check` are clean on the adopted file,
   apart from the documented `IX_Peoples_NameLower` exception below. SQLite query failures
   are reported as failures.
4. A second boot reports no repeated repair work, including version promotions, and
   produces the same normalised probe summaries.
5. For every movie, series and episode in the source fixture, populated metadata
   remains populated after adoption, after restart, and after an explicitly requested
   library scan. Both SQLite and the HTTP item responses are checked. A scan must
   produce a new successful completion record; an idle task or an old successful
   result does not pass this check.
6. Each user's watch history remains unchanged at all three stages, in both the
   database and movie/episode API responses, including watched flags and progress.
7. Every row of the Jellyfin-owned tables no other check covers survives each stage, bar
   the rewrites listed under the table digests below.
8. After the eleven paths, two **messy** fixtures are adopted (see [Messy rows](#messy-rows)):
   each shape of rows real databases carry ends as Jellyfin 12.2 left it, or as a divergence
   written down.
9. Last, a **scale** fixture of 40,000 items is adopted (see [Scale fixture](#scale-fixture)):
   every cloned item and the rows that hang off it survive, and the run records the migration
   time, peak memory, disk high-water mark and the cost of the `foreign_key_check`.

The table digests (`table_digests.py`) hash every row of those tables whole, keyed by its
primary key, and take the baseline from each fixture copy before Ferrofin starts. A row that
is gone, or whose hash moved, fails and names the table and key; a row only the later
database holds does not (a scan or login adds some), and neither does a column only the later
database holds (0032 adds some to MediaStreamInfos). The normaliser removes only what the
migration chain or the running server rewrites by design, and each rule is a change the check
stops seeing, so add one only with a migration or server behaviour to point at:

| Rule | Why |
|---|---|
| GUIDs compare as 32 lowercase hex digits | casing and dashes: 0007, 0032 |
| `ItemId` is the item's path when the item has an owner and no other item shares the path | the rehome repair re-keys a local alternate version (an owned item) to the id its primary's type derives, as Jellyfin 12.2 does; an unowned item re-keyed still fails |
| that item's id in its metadata folder (`/<2>/<id>/`) in `Chapters.ImagePath` | the same repair moves the item's metadata folder, chapter images included, to the new id, as Jellyfin 12.2 does; any other path change still fails |
| the listed timestamps compare as instants | 0007 rewrites ISO `T` dates to the space form |
| `ItemValues.CleanValue` is checked on its own: it must stay what it was or become what `Value` derives to | 0031 and the boot repair `repair_clean_values` (12.0's `RefreshCleanNamesAsync`) re-derive it with `get_clean_value`, ported in `clean_value.py` (needs PyICU) and proven against the Rust oracle; a value NULLed or corrupted by adoption still fails |
| a `Devices` row's `AppName`, `AppVersion`, `DeviceName`, `DateModified` and `DateLastActivity` are not compared, as is `ApiKeys.DateLastActivity` | every authenticated request re-announces them |
| a `Devices` row's `Id`, `AccessToken`, `IsActive` and `DateCreated` (as an instant, since 0007 rewrites it) are compared, except on the `DeviceId`s the harness logs in from (`synthetic-login-*`, `account-login-*`) | authenticating again from a device replaces its row with a new id, token, activation and creation date, as Jellyfin does; the harness does this only from its own `DeviceId`s, so on every other device a reset token or a deactivated device fails. `run.sh` logs in from none and compares them all |
| rows that share a key are a multiset, not one row | `(UserId, DeviceId)` is not unique, so losing or changing one of two devices for the same user and device must still fail |
| rows of a user that does not exist are not in the baseline of the user-keyed tables | 0044 deletes them |

The metadata baseline is captured from each fixture copy **before Ferrofin starts**.
It checks overview, original title, tagline, community/critic ratings, official rating,
year, premiere date, genres, studios, tags and production locations, plus provider-key
presence, cast/crew presence and image types. Item UUIDs are normalised across SQLite
and HTTP. Items omitted by browse filtering (such as alternate versions) are also
checked through their individual detail endpoints. Losing one item's metadata fails
even if another item's metadata is added.
Fields absent in Jellyfin are optional; valid provider updates may change values.
This checks presence, not exact text, provider-ID values, cast membership or image bytes.
At each stage, the suite downloads and decodes artwork for a fixed 10% sample of
movies and a separate 10% sample of series that had posters or backdrops in Jellyfin
(rounded up). Sorting the original item UUIDs keeps the sample identical across
stages and repeat runs. Each sampled item's poster and first backdrop are checked
where present, plus one episode poster, with at most four downloads/decodes running
at once. Pillow must decode the image pixels; an image content type alone does not
pass. Artwork records, descriptions and ratings are
still checked for every original item, including items outside the artwork sample.
The source must contain at least one movie with both an overview and a provider ID,
so an empty or filename-only fixture cannot pass.

Watch history is also captured before startup and checked after adoption, restart,
and the completed scan. Every existing `UserData` row must survive unchanged,
including provider-keyed rows and history belonging to other users. Database checks
cover all item types. API checks cover every movie and episode with stored history,
for every user: watched flags, play counts, resume positions, last-played dates, and
favorites must match the original fixture. Hidden alternate versions are checked
through their detail endpoints. These checks do not sample watch history.
Fixtures must include both watched videos and videos with resume progress, so an
empty history cannot make the gate pass.

The private watch-history baseline stores hashes of database rows and provider
keys, plus user/item UUIDs and the playback fields needed for API checks. It has
owner-only permissions, contains no titles or media paths, and stays outside the
repository. Failure messages contain field names and counts only.

Only fixture copies are scanned; source fixtures remain untouched. The only media
mounted is the generated `$FIXTURES/media`, read-only. The scan
keeps the fixture's provider settings, but the container's HTTP(S) requests use an
unreachable local proxy. Provider failures therefore cannot be hidden by a later
successful download refilling lost metadata, and the check does not depend on live
API keys or quotas. This tests retention during provider failure; provider enrichment
is covered separately by the server's controlled HTTP scan tests. Its default timeout is
600 seconds; set `ADOPTION_SCAN_TIMEOUT` for larger libraries. Set
`ADOPTION_WORK_DIR` to isolate a run from previous artifacts (default: `$FERROFIN_GATES_DIR/work/adoption`,
or `$FIXTURES/work/run` when no output root is set).
A timeout, failed scan, failed query, malformed response or inaccessible metadata
endpoint fails the run.

The new metadata reports contain field names and aggregate failure counts, with no
titles, paths, provider-ID values or credentials. The baseline file contains item UUIDs
and presence information only and is written with owner-only permissions. Existing
smoke outputs, server logs and database copies can still contain library details;
keep those local. No reporter library data is needed to test the harness.

These checks cover the synthetic library and the probe summaries; they do not assert
equality of every API field or cover every possible library configuration.

It is not a CI gate: it needs Docker and the built fixtures. Each generation takes
about two minutes for the boot probes, plus a library scan of the small synthetic
library. Run it before
any change to `crates/ferrofin-db/migrations/`, the adoption gate in
`database.rs`, or the boot repairs in `adoption_repairs.rs`, and before a release.

## The report

`gates/adoption/run.sh` and `python3 gates/adoption/synthetic.py run` each write a self-contained
`report.html` in their work directory (`$FERROFIN_GATES_DIR/work/adoption/` for `run.sh`, or
`DIR/work/run/` without an output root; `DIR/work/` for the preservation matrix; `ADOPTION_WORK_DIR`
moves the first). It opens from disk, has no external
asset, and shows:

- the run: image, Jellyfin oracle, duration, how many fixtures passed, failed or were skipped;
- per fixture, per stage (**after adoption**, **after restart**, **after scan**), **every check's
  verdict** and, for a failed one, why: boot log, answers against Jellyfin, `integrity_check` and
  `foreign_key_check`, second-boot repairs, metadata, watch history, table digests (and, for the
  matrix, user accounts, seeded states and artwork);
- the **table-digest differences** of a stage that lost or changed a row;
- for a messy fixture, **each shape's end state next to the one it must reach** and where that
  expectation comes from (Jellyfin 12.2's record, or the divergence written for it);
- for the scale fixture, the **measurements** (migration time, peak and anonymous memory, disk
  high-water, WAL peak, the `foreign_key_check` time) and which of them could not be taken.

Failed fixtures are open and passing ones collapsed. The checks are recorded one JSON line each in
`work/results.jsonl` as the run goes (`gates/adoption/adoption_report.py` is the recorder and the renderer), and the
page is built from that when the run ends, **however it ends**: a run that is terminated, or stops
on an error, still leaves its page, with the fixture it was on listed as unfinished (`INCOMPLETE`,
which fails the run) and, for the matrix, the error that stopped it as a failed check. A run
refused before it starts (a missing fixture directory, no docker) leaves nothing, and a matrix
refused before its first fixture starts (no oracle, a recipe change, a previous run's copy still in
`work/`) leaves the last report alone. The matrix has no messy or scale sections: those fixtures
are `run.sh`'s. A page embeds the first 500 table-digest differences of a stage and counts the rest
(the matrix also caps the reasons of a check and of a row at 500 each); `run.sh` keeps every difference in
`<fixture>.tables-*.log` beside the page, and `synthetic.py run` prints them in the error that ends
the run. Recording never changes a run's verdict or its exit status: an I/O or rendering problem
leaves the run going without a page, and `run.sh` says on stderr that none was written (the
recorder's own messages go to `/dev/null` there, so they never change what a run prints). When
stdout is a terminal, `run.sh` also prints the page's path at the end: the one line of stdout
recording adds, and only on a terminal.

`result.json` is written beside the page from the same reading of the run, in the envelope every gate
shares ([`gates/result.py`](../result.py)): the verdict, the rows counted (passed, failed, unfinished,
skipped), and per fixture its kind, expected generation, verdict, reason, every check and the scale
measurements. The table differences and the messy shapes stay in the page. The gates session page and
the release summary (`gates/publish.py`, the README's adoption grid) read it.

The shared viewer lists it in its Adoption tab and compares two runs side by side:

```bash
python3 gates/bench/report.py --serve      # run.sh's page as `adoption` ($FERROFIN_GATES_DIR/work) and every session's; the matrix: --adoption-dir DIR/work
```

The `Adoption` workflow uploads the two pages as the `adoption-report` (`run.sh`) and
`adoption-matrix-report` artifacts on every run, pass or fail (the whole work directory is still
uploaded as `adoption-work` on a failure).

## Messy rows

Long-lived databases carry rows no API produces, and the clean fixtures never hold them.
`messy.py` writes the shapes below by SQL into a stopped copy of a finished fixture, one function and
one manifest entry (`messy.json`) per shape:

| Shape | What is written |
|---|---|
| `deleted_user` | UserData, Permissions, Preferences, Devices and DisplayPreferences rows of a user that does not exist |
| `dangling_owner_extra` | a copy of an extra whose `OwnerId` names no item |
| `duplicate_paths` | two `BaseItems` rows with one `Path`, the copy being the older |
| `guid_casing` | two extras whose `OwnerId` and two videos whose `PrimaryVersionId` read as a lower-case dashed id and as 32 undashed characters, each naming an item that exists |
| `case_duplicates` | four independent pairs whose names differ only by case: a Person item, a Peoples row linked to a movie, an accented pair of both, and a music artist |
| `orphan_ancestor` | two `AncestorIds` rows with an end that names no item: one with a missing parent, one with a missing item |
| `virtual_season` | a season no folder backs (`Path` NULL) one past the series' highest, with an episode in it |
| `gone_file_userdata` | a movie whose file is not there and the play state two users recorded for it |
| `non_ascii_names` | three users whose names case mappings disagree on (Turkish dotted capital I, a sharp s, an e with a combining diaeresis, each, on 12.x, with a stored normalised name that is no normalisation of it), a person with a decomposed name, and a movie whose path has an accent, a decomposed accent and a symbol |

They go into `jellyfin-10.11.11-messy` and `jellyfin-12.2-messy`, copies of the latest 10.11.x and
the latest 12.x fixture (`jellyfin-12.2-from-121`), one per lineage: the 10.11 lineage runs 0032
and `FixIncorrectOwnerIdRelationships` on adoption, a 12.x database already has. The copies leave
the eleven clean paths and their smoke oracle as the control; `build-fixtures.sh` builds them last.

**An item needs its file.** Jellyfin 12.2 removes, when it upgrades a 10.11 database, every
non-folder item whose file is missing under a library location (`MigrateLinkedChildren`, see
[the stale-file sweep](#the-stale-file-sweep)). A seeded row with no file would be gone before its
shape showed anything, so every non-folder, non-virtual item a shape creates with a library path has a file: a hard link to its template's, in
`media-messy`, a hard-linked copy of the shared media tree that only the messy rows (and the oracle)
mount as `/media`. The shared tree, which the clean rows scan, gains nothing. `gone_file_userdata`
alone has no file, on purpose: that is its shape. The seeded rows are independent of each other: they
never share a path with another shape's rows, name an original another shape rewrites, or copy another shape's rows, because the
combined fixture is adopted and must end as each shape alone does (the build checks it).

The expectation per shape is what **Jellyfin 12.2 itself did**, not an assumption:
`messy.py build` pulls `jellyfin/jellyfin:12.2` (a stale local tag would record another release;
the image id goes into the record), boots it on a clean copy of the source carrying only that
shape, waits for its tasks to settle, and records the outcome and the end state read from the
database (`messy-oracle.json`). It then boots it once on a copy carrying every shape, because
Ferrofin adopts the combined fixture, and fails the build, naming both states, if that differs from
the single runs. A boot that neither answers nor exits within the deadline is an error, never a
recorded refusal; a failed build removes its own derived copy, leaving the clean fixtures as they
were. Each oracle boot's server log stays in `work/` as `messy-oracle-<fixture>-<shape>.server.log`,
the evidence behind a recorded outcome.

`run.sh` then adopts a fresh copy of each messy fixture with the image under test and fails when
a shape ends differently, when the boot log has an `ERROR`, when the file fails its integrity or
foreign-key check, when a second boot repairs again, or when the table digests (check 7) show a row
lost beside the messy ones. The digest baseline is taken, before Ferrofin starts, from the **clean
fixture the copy was made from**, not from the copy: an adoption rightly merges or removes some of
the messy rows (a merge drops a link, 0044 deletes a missing user's rows), and those must not read as
losses, while every row of the clean library must survive. A container that exits (a refused
database) ends the wait at once. Where Ferrofin is meant to end differently, `DIVERGENCES` in
`messy.py` names the state it must reach and why; a difference that is not written there fails, a
shape Jellyfin refuses needs a written expectation, and a fixture built with a different list of
shapes is refused rather than partly checked. People and artists are counted by name, not by id,
because Ferrofin deliberately re-keys a person (`unify_person_identities`). What Jellyfin 12.2 did:

| Shape | 10.11 lineage (upgraded by 12.2) | 12.x lineage (booted as is) |
|---|---|---|
| `deleted_user` | deletes Permissions and Preferences; keeps UserData, Devices, DisplayPreferences | keeps every row |
| `dangling_owner_extra` | deletes the extra | keeps it, dangling |
| `duplicate_paths` | deletes the copy | keeps both |
| `guid_casing` | upper-cases an owner id and a primary-version id; a 32-character owner becomes NULL, a 32-character primary version becomes a dashed upper-case id | keeps all four as written |
| `case_duplicates` | merges each pair to one (items, Peoples rows, artists) | keeps every row |
| `orphan_ancestor` | keeps both rows | keeps both rows |
| `virtual_season` | keeps the season and its episode | keeps both |
| `gone_file_userdata` | **removes the item** (the stale-file sweep) and keeps the play state, detached from it | keeps both |
| `non_ascii_names` | recomputes each normalised name as .NET's `ToUpperInvariant` (`STRAßE`, `İSTANBUL`, a decomposed `ZOË`); path and person kept | keeps the stale normalised names; path and person kept |

Where Ferrofin is written to differ (`DIVERGENCES`, each with its reason): every row of a missing user
is deleted on both lineages (migration 0044); on 12.x it removes a dangling-owner extra as on 10.11,
collapses a duplicate path with the ported routine, merges case-only duplicates (the routines Jellyfin
ran when it upgraded that database), re-spells an owner id in another case and nulls a 32-character
one (0045), and recomputes every user's normalised name; and on both lineages it deletes the
`AncestorIds` rows that name a missing item (0044: what the key's cascade would have removed).
`PrimaryVersionId` is left as written on 12.x, as Jellyfin leaves it; whether Ferrofin should re-spell it (its own lookups of it are byte-exact) is gl-752.

### The stale-file sweep

`gone_file_userdata` is the one shape whose difference from Jellyfin 12.2 is neither written down nor
decided. Jellyfin's upgrade routine `MigrateLinkedChildren` deletes the non-folder, non-virtual items
whose file no longer exists under a library location that is readable (with guards for an empty or
missing location), and keeps their play state; Ferrofin's 10.11 adoption has no counterpart and keeps
the item until a scan. The `jellyfin-10.11.11-messy` row therefore **fails** on this shape, naming both
states, until the decision recorded in the open issue gl-521 is made: port the sweep (the row then
passes unchanged), or record it as intentionally replaced by the first scan (a written divergence for
`("gone_file_userdata", "10.11")`, and the row passes).

## Scale fixture

The clean fixtures hold a few dozen items; a real library holds tens of thousands, and the
migration chain rebuilds `BaseItems` and its neighbours (0007, 0032) and the boot runs a
`foreign_key_check` over all of it. `scale.py build` derives `jellyfin-10.11.11-scale` from the
clean `jellyfin-10.11.11` (the 10.11 lineage runs the heavy migrations) and clones its media
items by SQL until `BaseItems` holds 40,000 rows (`ITEMS`); `build-fixtures.sh` builds it last.
Nothing is scanned and no file is written: scan cost at scale belongs to the benchmark corpus.

A clone is a movie, episode or song (one with no owner and no primary version, not an extra)
with a new id, a name carrying its number, and a path in a directory of its own that no file
has (`…/scale-00042/name.mkv`: a duplicate path would be collapsed by the very repair under
test). Every row that hangs off its template is copied with it: streams, attachments, chapters,
keyframes, trickplay, segments, images, providers, metadata fields, trailer types, ancestors,
item values and people; the values and people themselves are shared, as in a real library. One
clone in ten (`USERDATA_EVERY`) also carries the play state of every user the fixture holds any for
(two of its seven accounts), copied from its template's own row or, when the template has none, from
the fixture's first row for that user.
A table that gains a foreign key to `BaseItems` fails the build until `CHILDREN` names it, so
nothing goes uncloned unnoticed, and the clones must add no foreign-key violation.

`run.sh` adopts a fresh copy with the image under test, after checking that the disk can hold it
(`FREE_MULTIPLE` = 6 times the database, which covers the working copy, the `.pre-ferrofin` and
`.pre-00NN` snapshots, the WAL and the room a table rebuild takes, plus the rest of the fixture; the
row **fails** naming the shortfall rather than starting). The boot may take
`ADOPTION_SCALE_TIMEOUT` seconds to answer (default 1800), twice. It is held to the boot log
(generation, migrations, no `ERROR`), the file's integrity, a quiet second boot, the table
digests, and the survival of every cloned item and the rows hanging off it (`scale.py counts` and
`check`: a table that loses rows of a clone fails and is named). What it costs is **recorded, not
gated**, on the PASS line and in `scale-measurements-<fixture>.json` in the run's work
directory, which the cleanup of a passing row leaves alone:

| Number | How it is taken |
|---|---|
| migration wall time | the boot log: from `ferrofin-server starting` to `database migrations applied`. That window also holds the `.pre-ferrofin` copy, the rebuild snapshots, the dangling-row cleanup and the first `foreign_key_check`, because the database opens before the line is logged |
| peak memory | the container's cgroup `memory.peak` (cgroup v2, Linux 5.19+), read once a second. It counts the **page cache** of the database, its snapshots and its WAL, so it is not resident memory and moves with the host's cache pressure; beside it, the largest `anon` of the cgroup's `memory.stat` seen by the sampler is recorded as the closer match for resident memory |
| disk high-water | the allocated size of the whole data directory, sampled every `SAMPLE_S` = 1 s from the container's start until the first boot has settled, with the WAL's own largest size beside it; it includes `.pre-ferrofin` and the `.pre-00NN` snapshots |
| `foreign_key_check` cost | the time of `PRAGMA foreign_key_check` on the adopted file, taken once the boot has settled with the file warm in the page cache, which is what every later boot adds |

Disk, the WAL and the anonymous memory are sampled, so a short peak between two samples is missed
and each is a lower bound (the cgroup's own peak is not); a migration that finishes in a few seconds
gives only a few samples. A number that cannot
be taken (no cgroup v2, a log without the migration lines, a sampler that never ran, a WAL none of
the samples saw) fails the row naming it; it is never filled in, and a previous run's numbers are
deleted before the row starts so they cannot stand in. Nothing here is gated. `scale.py` needs
Python 3.11 or newer.

## The fixtures are synthetic

Every fixture is built from **synthetic data only**: no real library, personal media
server or network share is read, mounted or scanned. `build-fixtures.sh` runs the
[synthetic builder](#synthetic-preservation-fixture), which generates invented media
under `$FIXTURES/media` and has each official Jellyfin image build its own fixture through
its API. It then records the Jellyfin 12.1 and 12.2 oracles. `run.sh` and `synthetic.py`
use the same directory.

The fixture directory and the work directory must be on a **local disk**. Every fixture
is copied, booted and scanned several times, so `run.sh`, `build-fixtures.sh` and
`synthetic.py` refuse a directory on NFS, SMB/CIFS, 9p, Ceph, GlusterFS, AFS, WebDAV or
a network FUSE mount (sshfs, rclone, s3fs), judged by `findmnt` before anything is created.
Local FUSE disks such as ntfs-3g or mergerfs are accepted. Nothing under `gates/adoption/` contains
a database, and Git ignore rules
exclude the fixture directories and generated reports.

```
$FIXTURES/
  media/                       generated: invented video, audio, subtitles and images
  jellyfin-10.11.8/            built: Jellyfin 10.11.8 seeded through its API
  jellyfin-10.11.9/            built: jellyfin/jellyfin:10.11.9 booted and scanned on 10.11.8
  jellyfin-10.11.10/           built: the same with 10.11.10
  jellyfin-10.11.11/           built: the same with 10.11.11
  jellyfin-12.0/               built: jellyfin/jellyfin:12.0 on 10.11.8
  jellyfin-12.1-from-10/       built: jellyfin/jellyfin:12.1 on 10.11.8
  jellyfin-12.1-from-12/       built: jellyfin/jellyfin:12.1 on 12.0
  jellyfin-12.2-from-10/       built: jellyfin/jellyfin:12.2 on 10.11.8
  jellyfin-12.2-from-12/       built: jellyfin/jellyfin:12.2 on 12.0
  jellyfin-12.2-from-121/      built: jellyfin/jellyfin:12.2 on 12.1-from-12
  jellyfin-12.2-fresh/         built: jellyfin/jellyfin:12.2 seeded from scratch (a first run)
  oracle/smoke-jellyfin-12.{1,2}.txt  built: each release's own smoke answers
  oracle/smoke-jellyfin-12.2-fresh.txt  built: Jellyfin 12.2's answers on the fresh-install library
  oracle/user.txt              built: the account those answers were probed as
  jellyfin-12.2-schema.sql     built: the 12.2 schema the other 12.2 routes are diffed against
  work/                        scratch: synthetic.py's copies (and run.sh's in work/run/ when no output root is set); failures stay here
```

The probes run as the synthetic administrator, which has a **Jellyfin Web** session in
`Devices` and an **API key** in `ApiKeys`; `oracle/user.txt` records it, and `--user NAME`
or `ADOPTION_USER` overrides. Credentials are read from the copy at run time and never
written. The builder and runner need Docker, Python 3, Pillow (`python3-pil` on
Debian/Ubuntu), PyICU (`python3-icu`, for the `CleanValue` derivation), `sqlite3`, `jq` and
`curl`. Media encoding uses the Jellyfin image's FFmpeg.

```bash
export FERROFIN_GATES_DIR=/srv/gates                # the output root: absolute, a local disk, outside the checkout
gates/adoption/build-fixtures.sh                    # once; pulls 10.11.8–12.2 → $FERROFIN_GATES_DIR/fixtures/adoption-<key>
docker build -t ferrofin:bench .                    # the commit under test
gates/adoption/run.sh --image ferrofin:bench        # copies and report → $FERROFIN_GATES_DIR/work/adoption
python3 gates/adoption/synthetic.py run --image ferrofin:bench
gates/adoption/run.sh --only jellyfin-12.1-from-10  # one generation
```

Every location comes from the output root (the layout is in [`gates/lib.sh`](../lib.sh)). The
fixture directory is named for a hash of the files that build the fixtures (the synthetic recipe,
`smoke.sh`, `build-fixtures.sh`, `messy.py` and `scale.py`; `adoption_fixtures_dir` in `lib.sh`), so a
change to any of them builds into a new directory and a fixture from an older version is never
picked up. A new Jellyfin image under the same tag is not part of the name: build into a new
directory (`--fixtures`) after pulling one.
`--fixtures DIR` (or `FERROFIN_ADOPTION_FIXTURES`) and `ADOPTION_WORK_DIR` override the root; with
`--fixtures` and no root, the work directory is `DIR/work/run`. With neither, the scripts stop and
name `FERROFIN_GATES_DIR`.

A build that fails part-way, for example when Jellyfin resets a connection while seeding
(seen once under heavy host load), is not retried: seeding calls are not idempotent, so a
retry could change the baseline. The half-built fixture is refused; build into a new
directory (`--fixtures`), or move the half-built `fixtures/adoption-<key>` aside.

Full adoption validation requires **both** `gates/adoption/run.sh` and the
[synthetic preservation matrix](#running-the-synthetic-matrix), run against the same
fixture directory and the same Ferrofin image.

Output is one line per generation:

```
PASS  10.11.8   jellyfin-10.11.8
PASS  10.11.8   jellyfin-10.11.9
PASS  10.11.11  jellyfin-10.11.10
PASS  10.11.11  jellyfin-10.11.11
PASS  12.0.0    jellyfin-12.0
PASS  12.1.0    jellyfin-12.1-from-10
PASS  12.1.0    jellyfin-12.1-from-12
```

The second column is the generation the gate matched — the id *set*, so 10.11.9 reports
`10.11.8` and 10.11.10 reports `10.11.11`; `run.sh` knows which is expected for which fixture.
A `FAIL` line names every check that failed and points at the diff; the copy is kept in the
work directory (`$FERROFIN_GATES_DIR/work/adoption/`, or `work/run/` with `--fixtures` and no root) with `<name>.server.log`, `<name>.smoke.txt`, `<name>.smoke2.txt` (and, for a 10.11
path, the post-scan `<name>.smoke-scan.txt`) and the metadata
presence baseline `<name>.metadata.json` beside it. The per-stage
`<name>.metadata-after-{adoption,restart,scan}.log` files record the metadata results.
Watch-history baselines and results use the corresponding `.watch-history.json`
and `.watch-history-after-{adoption,restart,scan}.log` filenames.

## Tests

`gates/adoption/tests/adoption.bats` covers the harness itself without docker or fixtures: the
checks in `lib.sh` (log parsing, smoke normalisation and comparison, the SQLite checks, the
second-boot repair detection, the credential picker) run against canned logs, smoke outputs
and throwaway SQLite files, and the two entry points are exercised for their refusals and the
missing-fixture path. `tests/test_metadata.py` exercises the metadata checks using
invented library rows and a local HTTP server: partial/total metadata loss, missing
items, empty baselines, optional fields, valid updates, WAL visibility, HTTP failures,
batched API reads, scan completion/failure/timeout handling, reproducible artwork
sampling, corrupt image responses and description/rating loss. Bats invokes these
Python tests, so CI runs them too. `tests/test_watch_history.py` checks unchanged
history, changes to each playback field, deleted provider-keyed rows, progress for
another user, unwatched-to-watched changes, missing API history, hidden versions,
date/GUID normalization, private baselines, fixtures missing watched/resume data, and
failure reporting. Its fixtures contain only invented IDs and playback data.
`tests/test_table_digests.py` checks a lost row in each listed table, a changed column, a
lost and an added column, and a case per normaliser rule (each with the unrewritten change
next to it that must still fail). `tests/test_messy.py` checks each shape's seeder and end-state
reader on a scratch SQLite database, the comparison with Jellyfin 12.2's record, and that every
written divergence names a seeded shape, a derived lineage and a whole state.
`tests/test_scale.py` checks the cloning on a small library with the same table shapes
(round-robin templates, ids and paths of their own, every hanging row copied, the play-state
density, determinism, an unknown foreign key refused, a failed build cleaned up), the survival
check, the disk requirement, the sampler's high-water marks and the cgroup lookup with
substituted files, the boot-log timing and the record that names what it could not measure.
`tests/test_adoption_report.py` checks the recorder (a new run forgets the last, bad verdicts refused, a
line cut off by a killed run ignored), the verdict rules (a row that passed with a failed check is
failed; a fixture that began and never finished is `INCOMPLETE`; a run that ran nothing says so),
every section of the page, that run text cannot inject markup, that the page is self-contained,
the command line, and that the viewer's Adoption tab lists it. `adoption.bats` runs `run.sh` with a
fake docker to see the checks recorded in the page of a failing row, of a skipped one and of a
terminated run. CI runs the Bats tests with `bats gates/adoption/tests`
next to `scripts/tests`; locally `mise exec bats@latest -- bats gates/adoption/tests` or
a system `bats` works.

The 2026-09-16 validation also passed all **23 adoption harness tests** and **13 script
tests** (36 total), plus ShellCheck. The database crate passed **121 tests**, including
12.0 adoption, both 12.1 migration histories, incomplete-history rejection and schema
conformance; one performance test was ignored. These automated checks complement the
live matrix above. PR CI does not run the live fixture suite; the `Adoption` workflow runs
it on release branches (see [Running the synthetic matrix](#running-the-synthetic-matrix)).

## Adding a generation

When Jellyfin ships a release with new `__EFMigrationsHistory` ids: add a row to `VERSIONS` in
`synthetic.py` naming the fixture, the release and its parent fixture. Add its expected EF id count
to the check in `build-fixtures.sh`, and a row to `FIXTURE_TABLE` in `run.sh`. Build into a new
fixture directory (the recipe changes), refresh the oracle if that release changes answers, then
teach the gate (`JELLYFIN_GENERATIONS` in `crates/ferrofin-db/src/database.rs`). The oracle is always the
newest supported Jellyfin: parity is measured against the current release, not the one the
database came from.

## Why the checks are what they are

- **Generation in the log**, not just "it booted": a database adopted as the wrong generation
  baselines the wrong migrations and runs the wrong one-shot repairs (the 10.11.11 gate once
  skipped the playlist import for exactly that reason).
- **Jellyfin's answers as the oracle**: counts and first items are what a client shows; a
  migration that loses rows or hides them is invisible to a green test suite and obvious here.
- **Second boot**: every repair is keyed in `FerrofinMeta` and must not run twice; answers that
  move between two boots mean a repair is not idempotent.
- **`integrity_check` exempts `IX_Peoples_NameLower`**: it indexes `lower("Name")`, and a host
  `sqlite3` built with ICU lower-cases non-ASCII names differently from the SQLite inside
  Jellyfin and Ferrofin, so it reports rows "missing from index" on a file both servers agree
  with. Nothing else is exempt.

## Metadata gate validation

All **seven fixture paths passed the expanded metadata and artwork checks** on
**2026-09-29**, using local fixtures and Pillow for artwork decoding. Each baseline
contained **318 movies, 126 series and 8,878 episodes** (9,322 items). Database and HTTP
checks passed after adoption, restart and a completed scan. Each stage compared
against the original Jellyfin baseline and decoded artwork for the fixed 10% sample.

| Fixture | Adoption | Restart | Completed scan |
|---|---|---|---|
| `jellyfin-10.11.8` | PASS | PASS | PASS |
| `jellyfin-10.11.9` | PASS | PASS | PASS |
| `jellyfin-10.11.10` | PASS | PASS | PASS |
| `jellyfin-10.11.11` | PASS | PASS | PASS |
| `jellyfin-12.0` | PASS | PASS | PASS |
| `jellyfin-12.1-from-10` | PASS | PASS | PASS |
| `jellyfin-12.1-from-12` | PASS | PASS | PASS |

The local correctness image was `ferrofin:adoption-metadata-23`, image ID
`sha256:d85ab7a9861c8a4185ee690047bc12a6c78ee3e667eebee6bfaa4e6090d77a91`.
It packaged the debug server built from `ded26d35` with its host runtime libraries,
using the existing `ferrofin:bench` image for web assets and FFmpeg. This was a
correctness run, not a release performance measurement. The runs used the provider
outage configuration above, an isolated `ADOPTION_WORK_DIR`, and
`ADOPTION_SCAN_TIMEOUT=1200`.

All **26 Bats harness tests**, including **26 Python metadata tests**, passed, along
with ShellCheck and shell syntax checks. A separate native-server test checked a
synthetic movie after scan, restart and another scan, then deliberately cleared its
overview and confirmed that both the database and HTTP checks detected the loss.

The complete matrix was rerun on fresh copies after replacing the host FFmpeg
dependency with Pillow, using the same server image. The sample checks posters and
backdrops for 32 movies and 13 series, plus one episode poster.
The regression tests include explicit database and post-scan API checks for missing
descriptions and ratings, corrupt images, stable sampling and decoder failures.
Committed test data is synthetic; generated reports remain local and
excluded from Git.

## Watch-history gate validation

On **2026-09-29**, all seven fixture paths listed above passed the expanded suite
with watch-history checks after adoption, restart, and a completed scan. The run
used fresh fixture copies and the same `ferrofin:adoption-metadata-23` correctness
image described above. Every stage compared against the original Jellyfin history;
the database check covered every stored user-data row, and the API check covered
every movie/episode with stored history for every user.

All **27 Bats harness tests** passed, including **26 metadata tests** and **13 new
watch-history tests**, along with ShellCheck and shell syntax checks. Deliberately
clearing watched flags and resume positions in synthetic data made both database
and API checks fail. No production server or source fixture was scanned or modified
by this validation.

## Synthetic preservation fixture

`synthetic.py` builds every adoption fixture, small and complete, through Jellyfin's
own setup, library, user, metadata, and playlist APIs. It uses invented accounts,
titles, tags, provider IDs, solid-color images, silent video, and generated tones.
It neither reads nor modifies a personal media server.

The fixture has eight movie titles (one with two versions, one HDR10, one with two
audio languages and an attached font, one under a non-ASCII path), one extra, one
series with two seasons and six episodes, six music tracks across two artists and
two albums, a book and two photos. One album spans two discs. English subtitle files
accompany every movie and episode, every video has chapters, and every movie and
episode NFO credits two actors. Chapter-image and trickplay extraction run during
the scan, and the builder runs the keyframe-extraction task, saves display
preferences and a device name, and writes two media segments under the Intro
Skipper plugin's provider id (no API creates one). So every table the
[table digests](#2026-10-19-table-digests-check-7) compare holds rows on every
fixture (bar media segments on the fresh install, which skips `seed_database`), and the
builder fails when one of them is empty. Measured on 2026-10-20 on an idle host,
before the fresh-install row existed, this made a full `build-fixtures.sh` take 420 s instead
of 396 s (+6 %), and the fixture directory 29 MB instead of 18 MB.

Seven users provide an administrator, an adult with access to
every library, a child with library/parental restrictions, an administratively
disabled account, an account locked by failed logins, a passwordless account, and
a dedicated account with populated policy and configuration settings. Downloads
and deletion are disabled for the adult and child accounts. Each user has a
distinct generated avatar.

| Category | Checks |
|---|---|
| User permissions | Compare complete policies/configuration and stored account rows; compare adult/child browse visibility; reject cross-user history reads/writes, disabled downloads, and disabled deletion. |
| Playlists and collections | Compare exact membership, playlist order, owner and sharing; verify a shared reader can read but cannot edit, and cannot read a private playlist. Legacy `LinkedChildren` entries seeded into 10.11.8 (stale and live cached ids, unindexed and container-relative paths, `LibraryItemId` with and without a path, repeats) must resolve as Jellyfin 12.2's migration resolves them. |
| Manual metadata | Preserve a changed title, description, sort title, genres, chosen provider IDs, whole-item/field locks, and uploaded artwork. |
| Settings | Preserve audio/subtitle preferences, subtitle download languages and skip rules, provider selections and ordering, and library options. |
| Relationships | Preserve series/season/episode links, alternate media sources, extras, artist/album/track links, and disc/track numbers. |
| Derived views | Compare Continue Watching and Next Up item order, and series/season watched flags and unplayed counts for both users. |
| Unchanged second scan | SQLite triggers detect even same-value updates and identical delete/reinsert operations; provider counters and subtitle file hashes detect repeated requests, changed files, or duplicate subtitle files. |
| Music | Compare track, album and artist metadata and relationships, decode album covers, and preserve an ordered shared music playlist and stored favorites. |

Every stage compares against that **source version's Jellyfin responses**:
initial adoption, restart, a completed scan, and an unchanged second scan. The
existing database/API metadata, artwork, and watch-history checks also run at
every stage. Database integrity, expected adoption generation, and repair-free
restart checks remain in place.

The builder refuses to accept an empty scenario: restricted items must actually
be hidden, the uploaded image must be the chosen image, both users must have
resume/Next Up entries, music must contain the expected tracks/albums/artists,
and versions, extras, playlist/collection membership, and subtitles must exist.
Image comparisons allow small decoder rounding differences in the generated
solid colors. They still reject a different image or dimensions.

### User-account preservation

Every stage compares the complete `Policy` and `Configuration` objects for every
synthetic user. There is no field allowlist: newly returned fields automatically
join the comparison. The settings account currently covers **44 policy fields**
and **16 configuration fields**, with populated schedules, ordered library
preferences, tag restrictions, device/channel/folder selections, bitrate/session
limits, and a selected cast receiver. Fields Jellyfin itself leaves at defaults
are compared at those defaults.

The account checks also verify:

- The exact account list, original user IDs/names, and public-user visibility.
- Password login for the original administrator, adult, and child; an empty
  password for the passwordless account; and usable tokens for those same IDs.
- Disabled and locked accounts reject both correct and incorrect passwords.
  Lockout counters and disabled flags remain unchanged.
- Every user's avatar decodes to the expected pixels and dimensions without
  authentication, as on Jellyfin's login screen. User responses advertise an
  image tag, so clients know an avatar exists.
- Stable data in `Users`, `Permissions`, `Preferences`, `AccessSchedules`, and
  `ImageInfos` remains unchanged. Reports contain hashes of these rows, including
  password-bearing rows, rather than exposing their contents. Login/activity
  timestamps are excluded because the login probes advance them. EF bookkeeping
  and the derived `NormalizedUsername` migration column are also excluded.
  Detached permissions/preferences with no user are excluded because migration
  `0032` removes them. Losing an attached row or its owner still fails the check.

Account states are created through Jellyfin's APIs. The locked account is locked
by actual failed logins, and each login uses its own synthetic device identity
so switching test users cannot invalidate the administrator's session.

The harness now has **29 Bats tests**, including **88 Python tests**:
26 metadata, 13 watch-history, 23 preservation, and 26 user-account tests.
The account tests deliberately remove or alter account rows, passwords,
lockout counters, permissions, ordered preferences, schedules, avatar tags and
images, and login identities/tokens to verify that the checks fail. They also
verify that unlisted future policy/configuration fields are compared.

### Running the synthetic matrix

Requires Docker, Python 3, Pillow, PyICU (`python3-icu`), Bash, and jq. Media encoding uses
`/usr/lib/jellyfin-ffmpeg/ffmpeg` from the official Jellyfin container; no host
FFmpeg installation is needed. Docker must be available to the invoking user.
Use a fresh fixture directory on a local disk, outside the checkout.
`build-fixtures.sh` runs the build step below and adds the `run.sh` oracles:

```bash
gates/adoption/build-fixtures.sh            # or: python3 gates/adoption/synthetic.py build; both take --fixtures DIR
docker build -t ferrofin:bench .
python3 gates/adoption/synthetic.py run --image ferrofin:bench
```

The ten version/upgrade paths are the ones `run.sh` covers (`VERSIONS` in `synthetic.py`).
Each upgraded Jellyfin completes a scan before supplying its baseline. `--only`
selects one named fixture; building an upgraded fixture requires its parent to
have been built. `--port` changes the localhost-only test port (default 18120).
The builder records a recipe hash and refuses to reuse a completed fixture after
its recipe changes; build into a new directory then. Likewise, a build that fails
part-way, for example when Jellyfin resets a connection while seeding (seen once under
heavy host load), is not retried: seeding calls are not idempotent, so a retry could
change the baseline.

The runner copies each source into `work/`, mounts generated media read-only,
and blocks provider requests with an unreachable proxy. It configures an
invented OpenSubtitles key in the disposable copy so a mistaken subtitle search
is counted. Existing English subtitles should prevent those requests. There is
no reliance on a paid account or a live provider response.

A failed copy and its local reports remain for inspection. A successful copy is
removed; the Jellyfin source remains available for another run. No fixture
snapshots, media files, credentials, or per-item reports belong in Git.
Adoption remains one-way: Jellyfin is never started against an adopted copy.

The synthetic harness regression tests run through `bats gates/adoption/tests`, along
with the original metadata and watch-history tests. The PR CI does not encode or scan these
fixtures. The live matrix runs in the `Adoption` workflow (`.github/workflows/adoption.yml`)
on every push to a `release/**` branch, or by hand on any branch (one run per branch at a time:
a newer push replaces the run still waiting, never the one running): it builds the image from
the commit, restores the fixtures from a cache keyed on the recipe, `smoke.sh`,
`build-fixtures.sh` and the Jellyfin image digests (building them on a miss; GitHub scopes
the cache to the branch, so each new release branch builds once), and runs
`run.sh` and `synthetic.py run`. A failed run uploads `work/` as the `adoption-work`
artifact. Its green run on the frozen release commit is the release's adoption evidence,
linked from the test record above (`RELEASE.md`, "Validate adoption").

### Populated Ferrofin upgrade

`synthetic.py upgrade` checks that a release candidate keeps a populated install of an
earlier Ferrofin release: the previous release and v1.0.0, the oldest. The previous
release moves with each release. The release workflow (`release.yml`, step "upgrade a
populated install from earlier releases") runs it on every tag: it builds the
`jellyfin-10.11.8` fixture on the runner and upgrades from the latest published release
and from v1.0.0 to the pushed image (about two minutes). A failure (after the image is
pushed) blocks the GitHub Release and the chart, and uploads `work/` as the `upgrade-work`
artifact.

```bash
python3 gates/adoption/synthetic.py build --fixtures /path/to/fixtures --only jellyfin-10.11.8
python3 gates/adoption/synthetic.py upgrade --fixtures /path/to/fixtures --image ferrofin:bench \
  --from ghcr.io/mangoleaf/ferrofin:v1.3.2 --from ghcr.io/mangoleaf/ferrofin:v1.0.0
```

For each `--from` image, the check works on a copy of the `jellyfin-10.11.8` fixture,
the one generation every release adopts:

1. The earlier release boots on the copy, adopts it, and runs a scan.
2. Through that release's API, the check adds user state: a played movie, a resume
   position, a favourite, a user created with an avatar, and a user created then
   deleted. It reads that state back.
3. The candidate boots on the same data directory.
4. The `synthetic.py run` checks compare the candidate with what the earlier release
   stored and served, once after the upgrade and again after a scan. Those checks cover
   the account rows, watch history, metadata, and the API snapshot of visibility, views,
   playlists, collections and accounts.

Both sides are Ferrofin, so every comparison is exact, with these exceptions:
- **Seeds the start releases cannot boot on.** The copy loses the seeded dangling
  parent chain and the stale LinkedChildren entries first. Releases before v1.4.0 have
  no orphan clean-up and fail on both, on a foreign key. `synthetic.py run` covers the
  candidate on those seeds.
- **Changes the candidate's boot repairs make by design, following Jellyfin 12.2.**
  These are the seeded ratings' visibility, the harmonized conflicting user data, and the
  re-keyed id of the local alternate version, for which only the version count is
  compared. `synthetic.py run` holds each of these against Jellyfin 12.2.
- **Avatars.** They are fetched with the admin's token, because v1.0.0 serves them to no
  anonymous client.
- **A start release with a known API bug.** v1.0.0 lets a restricted user download an
  item (`KNOWN_API_GAPS` in `upgrade.py`), so it cannot serve the API snapshot. It gets a
  `NOTE` line, and the API snapshot comparison (visibility, denials, views, playlists,
  collections, account answers) is skipped for it. Its stored state, and the candidate's
  watch-history and metadata answers measured against that state, are still compared. Any
  other snapshot failure fails the run.

A failed copy keeps `start-served.json`, `<stage>-actual.json` and the server logs in
`work/` for inspection; a passing one removes them. `--only` picks another fixture, which
every `--from` release must be able to adopt (v1.0.0 adopts 10.11.8 only).

**2026-10-19 run.** The candidate was built from `release/v1.4.0` at `d2bb9306` (image
`sha256:6c46acb78fc140838dbc17ffe7088579a6b3efd8bc3d39d1053c608844c4f395`). The
`jellyfin-10.11.8` fixture was freshly built.

| Start release | After the upgrade | After a scan |
|---|---|---|
| v1.3.2 | PASS | PASS |
| v1.0.0 (stored state only, see the `NOTE` above) | **FAIL**: database and API missing item | PASS |

The v1.0.0 failure is a server bug, gl-663. v1.0.0 leaves a 10.11 local alternate version
as a standalone movie, and the candidate's boot deletes that row. Only the next scan
re-creates it.

### Initial synthetic matrix validation

On **2026-09-29**, all seven source paths passed every preservation category,
including music, against their own Jellyfin baselines:

| Fixture | Adoption | Restart | Completed scan | Unchanged second scan |
|---|---|---|---|---|
| `jellyfin-10.11.8` | PASS | PASS | PASS | PASS |
| `jellyfin-10.11.9` | PASS | PASS | PASS | PASS |
| `jellyfin-10.11.10` | PASS | PASS | PASS | PASS |
| `jellyfin-10.11.11` | PASS | PASS | PASS | PASS |
| `jellyfin-12.0` | PASS | PASS | PASS | PASS |
| `jellyfin-12.1-from-10` | PASS | PASS | PASS | PASS |
| `jellyfin-12.1-from-12` | PASS | PASS | PASS | PASS |

The local correctness image was `ferrofin:adoption-preservation`, image ID
`sha256:8e1fb626afc87277d264a04df1c14fb17eb18278d67339cedcbfe1f4e41d21ff`.
It packaged the debug server with the existing test image's Jellyfin FFmpeg and
web assets. Source fixtures were built through the actual Jellyfin releases;
only disposable copies were adopted. The original personal-library fixtures
and production server were not modified.

The new scenarios exposed and verified fixes for ignored download/deletion
permissions, missing maximum-parental-rating filtering, music artist links
pointing at value IDs instead of artist items, and scans duplicating adopted
alternate movie versions. A separate HTTP check verified that administrators
with downloading explicitly disabled also cannot download the file.

Validation also passed **7,347 workspace tests** (five skipped), three doctests,
all **28 Bats harness tests**, and all **62 Python checks** invoked by the harness
(26 metadata, 13 watch-history, 23 preservation). Core line coverage was **94.07%**
and API line coverage was **86.19%**. Formatting, workspace Clippy, ShellCheck,
and shell syntax checks passed.

Allowed file downloads over local HTTP had a median of **1.76 ms before** and
**1.53 ms after**, using 50 requests after five warmups on fresh copies of the
same synthetic source. The restricted adult changed from an incorrect HTTP 200
to HTTP 403, and an administrator with downloading explicitly disabled returned
HTTP 400. These are local debug-build measurements, not release benchmarks.

The larger generated benchmark fixture was also measured with
`gates/bench/run.sh --servers ferrofin --only loaded`, comparing the earlier
`ferrofin:adoption-metadata-23` image with the correctness image above. Background
load prevented the default CPU set from passing the 90% idle check. Both runs
used server CPUs `6,22` and client CPUs `7,23`, which passed that same check.

| Screen median (ms) | Before | After |
|---|---:|---:|
| Home | 292 | 293 |
| Movies | 159 | 157 |
| Detail | 35,997 | 37,669 |
| Series | 28,201 | 29,887 |
| Search | 31,302 | 32,641 |
| Playback | 43 | 44 |

**The loaded comparison is inconclusive:** these debug builds saturated the
limited CPU allocation at five screens per second. The baseline dropped 31
iterations and the updated build dropped 34, so these are not publishable
performance results or evidence that loaded performance is unchanged.

A separate serial HTTP comparison on that same larger generated library used
30 requests after five warmups, requesting 100 items with overview, genre, and
studio fields. Both builds returned identical page and total counts. These
servers used the host's available CPUs without the loaded run's CPU restriction:

| Listing median (ms) | Before | After |
|---|---:|---:|
| Movies | 45.95 | 39.06 |
| Music albums | 33.23 | 27.47 |
| Audio tracks | 42.75 | 37.50 |

This checks ordinary listing latency, including artist-link resolution, without
the loaded run's request backlog. It is a local comparison on a shared host;
it does not establish a release speedup or replace a valid loaded benchmark.

### Expanded user-account validation

On **2026-09-29**, freshly rebuilt fixtures with all seven account scenarios
passed **all seven source paths × four stages** again: adoption, restart,
completed scan, and unchanged second scan. Each path compared all 44 policy
fields and 16 configuration fields, account rows, original login identities,
disabled/locked login denials, public visibility, and all seven avatars against
its own Jellyfin baseline. The existing metadata, history, music, permissions,
relationships, and unchanged-scan checks also passed.

The correctness image was `ferrofin:adoption-accounts`, image ID
`sha256:25ce362d1f0ec254bb547f91a6ec95218542ad77be5ea143fb51bb1c0d653176`.
It used the existing Jellyfin FFmpeg and web assets. All new accounts and images
were generated; the personal-library fixtures and production server were not
modified.

These checks exposed missing profile-image tags and unauthenticated avatar
access, plus incorrect disabled/locked login responses and changing lockout
counters. Ferrofin now advertises avatars, serves them on the login screen,
invalidates cached user responses after avatar changes, and rejects disabled
logins with HTTP 403 without changing the lockout counter.

Validation passed **7,350 workspace tests** (five skipped), three doctests,
**29 Bats tests**, and **88 Python tests** (26 metadata, 13 watch-history,
23 preservation, 26 user-account). API line coverage was **86.20%** and core
line coverage was **94.10%**. Formatting, workspace Clippy, ShellCheck, and
shell syntax checks passed.

The preceding `ferrofin:adoption-preservation` image and this image were measured
with `gates/bench/run.sh --servers ferrofin --only loaded --rate 1` on the larger
generated library. Both used server CPUs `6,22`, client CPUs `7,23`, a 30-second
warmup, and a 120-second window. Both passed the 90% idle preflight. The baseline
completed 120 iterations and the updated build 121, with zero dropped iterations
or failed screen loads in either run.

| Screen median (ms) | Before | After |
|---|---:|---:|
| Home | 120 | 120 |
| Movies | 1,130 | 1,094 |
| Detail | 940 | 921 |
| Series | 268 | 271 |
| Search | 1,244 | 1,230 |
| Playback | 33 | 35 |

These are single debug-build runs on a shared host. CPU interference at p95 was
20% before and 19% after; home-screen p99 was 249 ms before and 422 ms after.
The measurements support comparison of ordinary request costs, while tail
latency needs repeated runs to separate code effects from host variation.

A separate serial HTTP comparison used fresh copies of the seven-account
synthetic fixture, 50 requests per operation after five warmups, and the host's
available CPUs. Requests checked the expected status, user identity, account
count, or avatar dimensions. Avatar requests used authentication in both builds because the
baseline incorrectly rejected anonymous reads. For uncached user responses,
saving the same policy cleared the cache before each timed request.

| Account operation, median / p95 (ms) | Before | After |
|---|---:|---:|
| List seven users | 1.25 / 1.71 | 1.44 / 3.59 |
| Current user, cached | 0.66 / 0.79 | 0.57 / 0.71 |
| Avatar, including decoding | 0.95 / 1.16 | 1.28 / 3.05 |
| Password login | 2,233.42 / 2,261.02 | 2,209.38 / 2,237.30 |
| Current user, uncached | 1.05 / 1.18 | 0.95 / 2.19 |

Password timings include verification of the same Jellyfin password hashes in
debug builds. The user-list and avatar medians increased by 0.20 ms and 0.33 ms;
cached and uncached current-user medians decreased. These measurements use the
same shared host as the screen comparison above.
