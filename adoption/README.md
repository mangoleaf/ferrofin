# Adoption smoke test

Ferrofin supports in-place adoption of Jellyfin **10.11.8, 10.11.9, 10.11.10, 10.11.11,
12.0.0 and 12.1.0** databases. Adoption is one-way; returning to Jellyfin requires restoring
a backup. Follow the [migration procedure](../docs/INSTALL.md#migrate-an-existing-jellyfin-installation)
for a real installation.

## Supported and tested versions

All seven fixture paths below **passed** the live suite on **2026-09-16 (US/Mountain)**.
Each test booted Ferrofin on a fresh copy of a real Jellyfin database from the same library.
The 10.11.8 fixture was the original snapshot; the others were produced by booting the
corresponding Jellyfin releases on copies, as shown below.

| Source Jellyfin version | Fixture upgrade path before adoption | EF migration IDs | Generation logged by Ferrofin | Live result |
|---|---|---:|---|---|
| 10.11.8 | Original 10.11.8 snapshot | 68 | `10.11.8` | PASS |
| 10.11.9 | 10.11.8 → 10.11.9 | 68 | `10.11.8` | PASS |
| 10.11.10 | 10.11.8 → 10.11.10 | 71 | `10.11.11` | PASS |
| 10.11.11 | 10.11.8 → 10.11.11 | 71 | `10.11.11` | PASS |
| 12.0.0 | 10.11.8 → 12.0.0 | 102 | `12.0.0` | PASS |
| 12.1.0 | 10.11.8 → 12.1.0 | 104 | `12.1.0` | PASS |
| 12.1.0 | 10.11.8 → 12.0.0 → 12.1.0 | 105 | `12.1.0` | PASS |

The gate in `crates/ferrofin-db/src/database.rs` matches migration **ID sets**, not just
their counts or a version string. Releases sharing a set share the logged generation.
Both 12.1 paths are accepted because the older rating-level migration ID is optional.
Unknown or incomplete histories are refused; the listed versions do not imply support
for other 10.11.x or 12.x releases. Fresh-install databases and other upgrade routes were
not separately exercised by this live run.

The tested server image was `ferrofin:adopt121`, supplied as built from server commit
`1732f97c`, with local image ID
`sha256:0b29af8f135a83d196329a0ada9f41f0ecd7d851e98dde9092d4dd01766ca42d`.
The run used the corrected harness that reports SQLite query failures and detects
alternate-version promotions even when `repaired` is zero. All seven fixtures passed
with exit status 0. Later server changes require rerunning this matrix.

## What the live suite checks

1. The boot log names the expected generation, reports completed database migrations and
   logs no `ERROR`.
2. The read-only probes in `smoke.sh` produce the same normalised summaries as **Jellyfin
   12.1** on the same library. IDs and image byte counts are normalised; server-info,
   task, plugin, device, virtual-folder, activity-log and session lines are excluded.
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

Only fixture copies are scanned; source snapshots remain untouched. All media mounts
must resolve for the scan, which can otherwise remove unavailable items. The scan
keeps the fixture's provider settings, but the container's HTTP(S) requests use an
unreachable local proxy. Provider failures therefore cannot be hidden by a later
successful download refilling lost metadata, and the check does not depend on live
API keys or quotas. This tests retention during provider failure; provider enrichment
is covered separately by the server's controlled HTTP scan tests. Its default timeout is
600 seconds; set `ADOPTION_SCAN_TIMEOUT` for larger libraries. Set
`ADOPTION_WORK_DIR` to isolate a run from previous artifacts (default: `$FIXTURES/work`).
A timeout, failed scan, failed query, malformed response or inaccessible metadata
endpoint fails the run.

The new metadata reports contain field names and aggregate failure counts, with no
titles, paths, provider-ID values or credentials. The baseline file contains item UUIDs
and presence information only and is written with owner-only permissions. Existing
smoke outputs, server logs and database copies can still contain library details;
keep those local. No reporter library data is needed to test the harness.

These checks cover the reference library and the probe summaries; they do not assert
equality of every API field or cover every possible library configuration.

It is not a CI gate: it needs a populated library and fixtures. Each generation takes
about two minutes for the boot probes, plus a complete library scan. Run it before
any change to `crates/ferrofin-db/migrations/`, the adoption gate in
`database.rs`, or the boot repairs in `adoption_repairs.rs`, and before a release.

## The fixtures are yours, not the repository's

Nothing under `adoption/` contains a database. You supply **one** Jellyfin 10.11.8 data
directory and the builder derives the rest with the official Jellyfin images:

Committed test cases use invented users, item IDs and metadata. Keep real fixture
snapshots and generated reports outside the checkout. Git ignore rules also exclude
the fixture directories, media mount scripts and generated metadata/smoke reports;
never force-add these private artifacts. Live baselines retain item UUIDs locally so
the same items can be checked across stages, but reports show only aggregate results.

```
$FIXTURES/
  jellyfin-10.11.8/            supplied: config/, data/jellyfin.db, root/, metadata/ …
  media-mounts.sh              optional: MEDIA=(-v /host/path:/container/path:ro …)
  jellyfin-10.11.9/            built: one boot of jellyfin/jellyfin:10.11.9 on 10.11.8
  jellyfin-10.11.10/           built: one boot of jellyfin/jellyfin:10.11.10 on 10.11.8
  jellyfin-10.11.11/           built: one boot of jellyfin/jellyfin:10.11.11 on 10.11.8
  jellyfin-12.0/               built: one boot of jellyfin/jellyfin:12.0
  jellyfin-12.1-from-10/       built: one boot of jellyfin/jellyfin:12.1 on 10.11.8
  jellyfin-12.1-from-12/       built: one boot of jellyfin/jellyfin:12.1 on 12.0
  oracle/smoke-jellyfin-12.1.txt  built: Jellyfin 12.1's own smoke answers
  oracle/user.txt              built: the account those answers were probed as
  work/                        scratch; a failed fixture's copy, logs and smoke output stay here
```

Take the snapshot with the server stopped, or copy the database with
`sqlite3 jellyfin.db ".backup /snapshot/data/jellyfin.db"` so the WAL is folded in. Bring
`root/` (library definitions) and `metadata/` (images) along with `data/`; without them the
library list is empty and every poster is a placeholder, which the probes will notice. The
media paths referenced by the library options must resolve inside the container, hence
`media-mounts.sh`. The probes need an administrator with a **Jellyfin Web** session in `Devices`
(the builder records which account the oracle ran as in `oracle/user.txt` and the runner
probes every fixture as that account; `--user NAME` or `ADOPTION_USER` overrides) and an
**API key** in `ApiKeys`. Credentials are read from the copy at run time and never written.
The runner also requires Python 3 and Pillow for metadata checks and image decoding
(`python3-pil` on Debian/Ubuntu). The artwork checker does not invoke FFmpeg.

```bash
adoption/build-fixtures.sh --fixtures /path/to/fixtures     # once, ~25 min, pulls 10.11.9–12.1
docker build -t ferrofin:bench .                            # the commit under test
adoption/run.sh --fixtures /path/to/fixtures --image ferrofin:bench
adoption/run.sh --fixtures … --only jellyfin-12.1-from-10   # one generation
```

Full adoption validation requires **both** `adoption/run.sh` and the
[synthetic preservation matrix](#running-the-synthetic-matrix). The original
runner does not invoke `adoption/synthetic.py`; use separate fixture directories
and run both against the same Ferrofin image.

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
A `FAIL` line names every check that failed and points at the diff; the copy is kept under
`work/` with `<name>.server.log`, `<name>.smoke.txt`, `<name>.smoke2.txt` and the metadata
presence baseline `<name>.metadata.json` beside it. The per-stage
`<name>.metadata-after-{adoption,restart,scan}.log` files record the metadata results.
Watch-history baselines and results use the corresponding `.watch-history.json`
and `.watch-history-after-{adoption,restart,scan}.log` filenames.

## Tests

`adoption/tests/adoption.bats` covers the harness itself without docker or fixtures: the
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
CI runs the Bats tests with `bats adoption/tests`
next to `scripts/tests`; locally `mise exec bats@latest -- bats adoption/tests` or
a system `bats` works.

The 2026-09-16 validation also passed all **23 adoption harness tests** and **13 script
tests** (36 total), plus ShellCheck. The database crate passed **121 tests**, including
12.0 adoption, both 12.1 migration histories, incomplete-history rejection and schema
conformance; one performance test was ignored. These automated checks complement the
live matrix above; CI does not run the real-library fixture suite.

## Adding a generation

When Jellyfin ships a release with new `__EFMigrationsHistory` ids: add a builder step that
boots that image on the right parent fixture, add a row to `FIXTURE_TABLE` in `run.sh`, refresh
the oracle if that release changes answers (delete `oracle/` and rebuild), then teach the gate
(`JELLYFIN_GENERATIONS` in `crates/ferrofin-db/src/database.rs`). The oracle is always the
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
Committed test data is synthetic; real snapshots and reports remain local and
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

The original library snapshot does not need parental controls, playlists, manual
edits, or music. `synthetic.py` builds a separate, small fixture through Jellyfin's
own setup, library, user, metadata, and playlist APIs. It uses invented accounts,
titles, tags, provider IDs, solid-color images, silent video, and generated tones.
It neither reads nor modifies a personal media server.

The fixture has five movie titles (one with two versions), one extra, one series
with two seasons and six episodes, and six music tracks across two artists and
two albums. One album spans two discs. English subtitle files accompany every
movie and episode. Seven users provide an administrator, an adult with access to
every library, a child with library/parental restrictions, an administratively
disabled account, an account locked by failed logins, a passwordless account, and
a dedicated account with populated policy and configuration settings. Downloads
and deletion are disabled for the adult and child accounts. Each user has a
distinct generated avatar.

| Category | Checks |
|---|---|
| User permissions | Compare complete policies/configuration and stored account rows; compare adult/child browse visibility; reject cross-user history reads/writes, disabled downloads, and disabled deletion. |
| Playlists and collections | Compare exact membership, playlist order, owner and sharing; verify a shared reader can read but cannot edit, and cannot read a private playlist. |
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

Requires Docker, Python 3, Pillow, Bash, and jq. Media encoding uses
`/usr/lib/jellyfin-ffmpeg/ffmpeg` from the official Jellyfin container; no host
FFmpeg installation is needed. Docker must be available to the invoking user.
Use a fresh fixture directory outside the checkout:

```bash
python3 adoption/synthetic.py build --fixtures /path/to/synthetic-fixtures
docker build -t ferrofin:bench .
python3 adoption/synthetic.py run --fixtures /path/to/synthetic-fixtures --image ferrofin:bench
```

The seven version/upgrade paths are the same as the original adoption matrix.
Each upgraded Jellyfin completes a scan before supplying its baseline. `--only`
selects one named fixture; building an upgraded fixture requires its parent to
have been built. `--port` changes the localhost-only test port (default 18120).
The builder records a recipe hash and refuses to reuse a completed fixture after
its recipe changes; build into a new directory then.

The runner copies each source into `work/`, mounts generated media read-only,
and blocks provider requests with an unreachable proxy. It configures an
invented OpenSubtitles key in the disposable copy so a mistaken subtitle search
is counted. Existing English subtitles should prevent those requests. There is
no reliance on a paid account or a live provider response.

A failed copy and its local reports remain for inspection. A successful copy is
removed; the Jellyfin source remains available for another run. No fixture
snapshots, media files, credentials, or per-item reports belong in Git.
Adoption remains one-way: Jellyfin is never started against an adopted copy.

The synthetic harness regression tests run through `bats adoption/tests`, along
with the original metadata and watch-history tests. The live Docker matrix is a
separate command, so routine CI does not encode or scan these fixtures.

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
`bench/run.sh --servers ferrofin --only loaded`, comparing the earlier
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
with `bench/run.sh --servers ferrofin --only loaded --rate 1` on the larger
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
