# Unchanged cast images during provider refreshes

## Reproduction and fix

A full movie metadata refresh re-fetches its credits. `enrich_people` reuses cached
profile files and their dimensions/blurhash, but calls `save_item_images` anyway.
That save previously deleted every image row for the person and inserted new rows
with new IDs, even when every persisted field was identical.

The real-server HTTP scan matrix reproduced this: its "Search for missing metadata"
step logged DELETE and INSERT operations on Ada Actor's `BaseItemImageInfos` row.
The new assertion fails before the fix and checks subsequent full refreshes too.

`save_item_images` now synchronizes the supplied image set:

- Identical rows are retained with their IDs and ordering intact.
- Changed dimensions, blurhash or modification time update just that image row.
- New paths/types are inserted; only images absent from the new set are deleted.
- Matching consumes rows one at a time, including duplicate paths. Exact matches
  are preserved before matching changed rows, so duplicates cannot steal one
  another's unchanged entries.
- Modification times compare at the database's 100 ns precision, preventing
  repeated writes caused by higher precision in the caller's timestamp.

The comparison and writes share a `BEGIN IMMEDIATE` transaction. Taking the writer
before reading prevents another writer from invalidating a deferred read snapshot.
This adds a read to image saves; identical saves execute no image-table mutations.
The scanner already skips saves for unchanged movie artwork, so that path stays
unchanged. Other callers of this image persistence method gain the same behavior.

## Validation

SQLite trigger tests distinguish a real no-op from deleting/reinserting identical
values or issuing an unconditional UPDATE. They cover every metadata field,
reordered inputs, duplicate paths, additions, removals, path/type replacement and
clearing an already empty set. The HTTP matrix checks the actual movie-to-cast
refresh path, including both missing-metadata and replace-all refresh modes.

Checks use `RUSTC_WRAPPER=` and this worktree's isolated build directory.

- `cargo nextest run -p ferrofin-core -p ferrofin-db --no-fail-fast`: 1,948 passed,
  1 skipped, including the SQL boundary check and the image mutation tests.
- `cargo test -p ferrofin-server --test scan_change_detection -- --nocapture`:
  the HTTP matrix passed, including the metadata outage and subtitle regressions.
- `cargo clippy -p ferrofin-core -p ferrofin-server --all-targets --all-features -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

- Core coverage: 93.96% line coverage; 1,827 tests passed in the coverage run.
  The 80% per-crate gate was checked with dependencies excluded from the report:

  ```sh
  RUSTC_WRAPPER= cargo llvm-cov nextest -p ferrofin-core --summary-only \
    --ignore-filename-regex 'ferrofin-(common|db|drawing|keyframes|model|naming|networking|providers|traits|util)/' \
    --fail-under-lines 80
  ```

The full workspace suite was not run.


## Performance

The HTTP reproduction reduced writes per unchanged cast photo from two
(DELETE + INSERT) to zero. The existing scan benchmark showed no meaningful
wall-time regression. Four runs per version, alternating order, with debug test
binaries captured before and after this change and no concurrent validation jobs:

| Median wall time | Before | After |
|---|---:|---:|
| First scan | 1.1425 s | 1.1415 s |
| Rescan | 0.7070 s | 0.6995 s |

The fixture contains 300 movies and 10 series of 10 episodes (420 items including
series and seasons). It runs without ffprobe or remote providers, so it measures
local scan overhead, not provider latency. Outcomes were identical between builds.

```sh
FERROFIN_SCAN_BENCH=1 FERROFIN_SCAN_BENCH_MOVIES=300 \
FERROFIN_SCAN_BENCH_SERIES=10 FERROFIN_SCAN_BENCH_EPISODES=10 \
RUSTC_WRAPPER= cargo test -p ferrofin-server --test scan_bench -- --nocapture
```

Session artifacts: `/tmp/ferrofin-cast-scan-bench-before`,
`/tmp/ferrofin-cast-scan-bench-after`, and `/tmp/ferrofin-cast-images-bench.log`
(with a `.json` summary alongside it).
