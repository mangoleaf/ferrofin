# Extras library browse fix (#32)

## Reference and baseline

Baseline: Ferrofin `b0e083c6` (v1.3.0 plus #27).
Reference source: Jellyfin tag `v12.1`, commit
`ee91c75e777da41a9c4f4855e70adc604fbf2ef8`.

The source comparison establishes these requirements:

- `BaseItem.RefreshExtras` sets `OwnerId` and clears `ParentId`.
- `BaseItem.GetTopParent` walks physical parents. Parentless extras therefore
  have no `TopParentId` when `ItemPersistenceService` saves them.
- `BaseItem.GetAncestorIds` includes collection folders. `LibraryManager.
  GetCollectionFolders` follows `OwnerId` when a parent is absent, so retaining
  the owning collection folder in the ancestor table is appropriate.
- `LibraryManager.FindExtras` resolves candidates through `ResolvePath`, which
  applies the ignore rules. `sample.mkv` and `*.sample.mkv` are ignored;
  `*-sample.mkv`, `*_sample.mkv`, and files in `samples/` can be owned extras.
  A blanket exclusion of `ExtraType.Sample` would discard supported media.
- Extras naming rules recognize `Extras/` as `Unknown` (0), including files
  whose names look like trailers inside that folder. Rule order is significant.

The regression tests exercise unfiltered direct and recursive browsing, owner
lookup, locked existing rows, and the distinction between ignored sample names
and supported Sample extras. The live Jellyfin 12.1 comparison below confirms these expectations.

## Execution record

### Phase 1: regression coverage

Added `library_scan_extras.rs`. The baseline is expected to fail the ownership,
existing-row repair, and ignored-path tests. No production code changes in this
phase.

### Runtime prerequisites

Docker is unavailable in this environment. A temporary .NET 10 SDK built the
exact Jellyfin reference source under `/tmp`. The existing synthetic adoption
suite runs against the native Ferrofin binary through a temporary adapter using
`bwrap` to preserve `/config`, `/cache`, and `/media` paths. Only disposable
fixture copies are mutated; server discovery is disabled in those copies to
avoid a shared UDP port. The preservation assertions are unchanged.

### Phase 2: ownership and lifecycle

Extras now have no physical parent or top parent. Their collection ancestor is
retained for owner-library policy and progress accounting. Native recursive
library browsing now uses the library's top-parent scope, matching the adopted
library path. General extras-query predicates are unchanged.

Pruning reads parentless extras through the owner's library, with index seeks
on the owner's TopParentId and the extra's OwnerId. Scoped scans seek the
extra's Path and check its owner by primary key, keeping watcher work bounded. Rows returned through both legacy and owned membership
are deduplicated. Owner deletion already follows OwnerId and needs no change.

The baseline's three regression tests failed as expected. After the ownership
fix, four ownership/lifecycle integration tests pass, including repair of a
locked unchanged extra and stable DateLastSaved on a second scan. Existing
row comparison and structural overlays already perform the required repair;
no new refresh trigger or provider pass is needed. Ignore discovery remains
for phase 3. An EXPLAIN test guards the new pruning query's index seeks.

### Phase 3: discovery and owner eligibility

The planner filters media entries through the existing ignore rules. Location
availability is recorded from the raw listing first, so exclusions cannot make
a mounted location look unavailable. Provider filesystem reads remain raw for
artwork and sidecars. Full and scoped discovery share the same planner.

Movie-folder recognition now uses VideoListResolver's version grouping and
MovieResolver's sample regex. A root/mixed folder or a directory with ordinary
subfolders does not supply an extras owner. This follows
BaseItem.SearchesContainingFolderForExtras; it also sets IsInMixedFolder and
uses the filename for movies that do not have their own folder.

Seven integration tests pass, including scoped ignored-file discovery with NFO
preservation and nested/multiple-movie ownership. A naming test covers the
reference sample regex's word boundaries.

### Live Jellyfin 12.1 reference

The Docker limitation was worked around by installing a temporary .NET 10 SDK
under `/tmp` and building the exact `v12.1` source tag. The resulting server
reports 12.1.0. A disposable server on port 18132 scanned generated one-second
media with internet metadata disabled. Local results are under
`/tmp/ferrofin-extras-oracle/` (reference.json and extended.json).

Observed in a single-movie directory:

| Path relative to the movie folder | Stored kind / ExtraType |
| --- | --- |
| `Solo.mkv` | Movie / null |
| `Extras/Deleted.Scenes.avi` | Video / 0 |
| `Solo-trailer.mkv` | Trailer / 2 |
| `Solo-sample.mkv` | Video / 7 |
| `Solo_sample.mkv` | Video / 7 |
| `samples/clip.mkv` | Video / 7 |
| `sample.mkv`, `Solo.sample.mkv` | Absent |

All retained extras have an owner and null ParentId/TopParentId. Hidden files
and ignored directories were absent. In a directory containing a nested release,
Jellyfin marked the outer movie as mixed and did not create its extras; the
nested movie retained its own suffix sample. These observations match the new
ownership tests. Jellyfin also preserves physical folder rows in that layout;
Ferrofin's existing flattening of ordinary movie subfolders is a separate
hierarchy mismatch, so whole-library folder counts are not asserted equal.

Phase 3 review caught owner registration treating each stacked part and version
as an unrelated movie. Registration now uses the grouped title returned by the
naming resolver. Generic extras choose the primary; named extras choose the
longest version prefix ending at a delimiter, as `Video.GetOwnerIdForExtra` does.
Adopted Video identities are reused for owners even outside a scoped refresh.
All nine extras integration tests pass (full/scoped versions, stacks, adopted
identities included). The second independent review approved this phase.

## Phase 4 — reconcile confirmed exclusions

The planner records excluded entries only after successful directory listings,
and records extra candidates whose listed containing folder has no eligible
owner. Pruning considers these paths even when their files still exist. Scope,
unlisted/unavailable locations, cancellation, library roots, and retained-child
cascade guards still apply. Membership uses path ancestors in a hash set, so it
does not multiply existing rows by the number of excluded paths.

The twelve extras tests pass, including exact-path and folder cleanup,
ownerless legacy rows, retained Sample extras, failed listings, missing and empty
mounts, cancellation after planning, and a stable second scan. Repairing a locked
extra retains its ID, overview, artwork row, provider ID, and played state.
Upgrade instructions now describe recovery with one normal scan. No schema
migration or file deletion is involved.


## Final validation (2026-10-01)

Production changes end at `892fa79f`; `3568e5b1` corrects two old repository test
fixtures that omitted the scanner's `TopParentId` relationship. The recursive
fixture now checks both native-library scope and ordinary-folder ancestry.
Each phase and the complete production diff passed independent review using
`.claude/skills/review-loop/SKILL.md`; the test corrections were reviewed too.

Automated checks:

- `cargo build --offline --workspace`: passed.
- `cargo fmt --all --check`: passed.
- `cargo clippy --offline --all-targets --all-features -- -D warnings`: passed.
- `cargo nextest run --offline --workspace --no-fail-fast --test-threads 8`: **7,369 passed, 5 skipped**. The initial run found the two malformed repository fixtures; the complete rerun passes after their correction.
- `cargo test --offline --workspace --doc`: three doctests passed.
- Separate `cargo llvm-cov nextest -p <crate> --fail-under-lines 80 --summary-only`
  runs: naming **96.79%** lines, core **94.14%** lines. All 1,902 core tests passed
  in the coverage run. Coverage uses a fresh `CARGO_LLVM_COV_TARGET_DIR` under
  this worktree's target directory; the initial inherited-cache report was
  discarded. `RUSTC_WRAPPER=` disables the unavailable sccache wrapper.

### Real HTTP extras checks

`/tmp/ferrofin-extras-http.py` compares the exact Jellyfin 12.1 source build with
native binaries built from an archive of `b0e083c6` and this fix. Their build IDs
are explicitly stamped through `FERROFIN_GIT_DESCRIBE`.

The affected baseline imported eight library children from the simple fixture.
After one ordinary scan, direct browsing returns only the movie. Recursive
media membership and Latest match Jellyfin. Both servers return four Special Features,
one Local Trailer, and one theme song. All six extras have null physical parent
and top parent, keep their collection ancestor, return playback sources, and
serve the complete original bytes through Download. A follow-up check,
`/tmp/ferrofin-extras-download-hashes.py`, confirms source/download SHA-256
equality for all six extras in both the upgraded and fresh fixtures (12 checks;
`/tmp/ferrofin-extras-download-hashes.log`). Fresh install, upgrade,
restart, second scan, database integrity, and foreign keys pass. Every excluded
media file remains on disk.

The existing suffix Sample extra keeps its ID, locked overview, played flag,
play count 3, favorite flag, and resume position 123. A subsequent scan leaves
all media rows, including DateLastSaved, unchanged. The automated repair test
also checks retained artwork and provider IDs.

Jellyfin's raw recursive response includes its physical library root Folder;
Ferrofin's native library has no equivalent row. HTTP comparison excludes that
root container while retaining all media entries. This existing hierarchy
mismatch is distinct from extras leaking into browse and is not claimed fixed.
The reporter's database and exact 21 paths were unavailable; these checks use
invented media and the supported synthetic adoption fixtures.

### Synthetic adoption matrix

All seven paths passed **adoption, restart, scan, and unchanged second scan**:

| Source fixture | Result |
| --- | --- |
| Jellyfin 10.11.8 | PASS |
| Jellyfin 10.11.9 | PASS |
| Jellyfin 10.11.10 | PASS |
| Jellyfin 10.11.11 | PASS |
| Jellyfin 12.0 | PASS |
| Jellyfin 12.1 from 10.11.8 | PASS |
| Jellyfin 12.1 through 12.0 | PASS |

The unchanged `adoption/preservation.py` assertions cover metadata, artwork
bytes decoded as images, account settings and credentials, watch history,
permissions, library options, subtitles, versions, extras, collections,
playlists, resume, and next-up views. The second scan checks database writes
with triggers, provider-request counters, and subtitle-file hashes. Integrity,
foreign keys, generation detection, and repair-free restart checks also pass.

Command: `python3 /tmp/ferrofin-extras-native-matrix.py`, using source snapshots
under `/tmp/ferrofin-account-complete` and disposable copies under
`/tmp/ferrofin-extras-matrix`. The adapter changes process startup, restart,
mounts, and log collection; it does not bypass preservation assertions.
Summary log: `/tmp/ferrofin-extras-matrix.log` (28 PASS stages, exit 0).

### Before/after performance

`python3 /tmp/ferrofin-extras-perf.py` ran three alternating baseline/fix pairs
against native debug builds on the same host, after the other validation jobs
finished. Both databases contain the same 200 movies and 400 valid extras.
Each HTTP endpoint gets 10 warm-up calls followed by 100 measured calls per
run. Movie pages, Latest, and LocalTrailers return 100, 20, and 1 records
respectively on both builds. Build IDs are checked through `/health/live`.

Values are the median of the three runs; brackets show their range, in ms.

| Measurement | `b0e083c6` | Fix `892fa79f` |
| --- | ---: | ---: |
| Movie-page p50 | 11.89 [10.52–12.19] | 11.61 [10.32–11.93] |
| Movie-page p95 | 17.60 [13.45–18.09] | 18.28 [13.77–18.44] |
| Latest p50 | 10.73 [9.93–11.93] | 9.27 [8.00–9.68] |
| LocalTrailers p50 | 2.39 [2.29–2.40] | 2.42 [2.30–2.55] |
| Unchanged scan, server task elapsed | 356 [351–372] | 376 [370–378] |

The unchanged scan adds 20 ms at this fixture size. HTTP timing shows no large
slowdown in the tested paths; these small debug-build measurements do not
establish performance under production load. The scan duration comes from the
server's `RefreshLibrary` task log, excluding the client's polling delay.
Raw endpoint measurements: `/tmp/ferrofin-extras-perf/results.json`; server task
measurements: `before.log` and `after.log` in the same directory.

This uses native HTTP because Docker is unavailable; the standard `bench/`
container/load suite was not run. The targeted EXPLAIN regression test separately
guards owner-scope and path-scope pruning against table scans.

## Follow-up: client visibility and documentation audit

The original HTTP validation missed the movie-detail count fields used by
Jellyfin web. See [the follow-up audit](extras-client-visibility.md) for the
`c9b9f854` count fix, real browser playback validation, and remaining extras
parity gaps. The checks above establish the browse repair; they do not establish
full parity with Jellyfin's documented extras behavior.
