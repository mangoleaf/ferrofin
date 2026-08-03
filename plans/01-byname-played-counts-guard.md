# Plan 1 — Stop computing always-zero played/total counts for by-name rows

## Problem
`GET /Studios`, `/Genres`, `/Persons`, `/Years` are 2–8× slower than both Jellyfin and
Hermit v0.6.1 (benchmark: studios p50 was 191 ms at v0.6.1, ~450–1550 ms now; Jellyfin
does ~210 ms). Root cause chain:

- Commit `8451f23` wired folder `UserData.UnplayedItemCount` into the DTO builder: every
  folder row on a list page triggers played/total leaf-descendant counting.
- Commit `e9d2110` batched that into two grouped joins over `AncestorIds × BaseItems
  (× UserData)` — `get_played_and_total_count_batch` in
  `crates/hermit-core/src/item_count_service.rs:290`.
- By-name rows (Studio/Genre/MusicGenre/Person/Year) are stored with `IsFolder = 1`, so
  they pass the filter at `crates/hermit-core/src/dto_service.rs:1522-1544` (which only
  excludes `CollectionFolder`/`UserView`). But by-name items **never appear as
  `AncestorIds.ParentItemId`** — they have no descendant closure — so the two aggregate
  scans run on every request and provably return zero for every row. Under the
  benchmark's 50 VUs on a 4-connection SQLite pool, these scans hold connections and
  convoy everything else.

The same pattern exists for `get_child_count_batch` at `dto_service.rs:1496-1519`
(gated on `ItemFields::ChildCount`) — same folder filter, same wasted work for by-name
rows.

## Fix
1. **First, establish the parity oracle.** Check what Jellyfin actually emits for
   `UserData.UnplayedItemCount` (and `ChildCount`) on by-name rows (`/Studios`,
   `/Genres`, `/Persons`, `/Years` list responses). Use the parity harness
   (`benchmark/parity.sh`, see `benchmark/README.md`) or curl both servers. In upstream
   C#, `Genre`/`Studio`/`Person`/`Year` are `BaseItem` + `IItemByName`, **not** `Folder`
   subclasses, so the C# `AttachUserSpecificInfo` folder branch should not fire for
   them at all. Confirm.
2. In `dto_service.rs`, extend the folder filters (both the `child_counts` block
   ~1496-1519 and the `played_counts` block ~1522-1544) to also exclude the by-name
   kinds that cannot have `AncestorIds` descendants: `Genre`, `MusicGenre`, `Studio`,
   `Person`, `Year`. Match whatever the parity oracle from step 1 says — if Jellyfin
   doesn't emit these fields for those kinds, Hermit must not either (don't emit 0;
   omit). If Hermit currently emits them and Jellyfin doesn't, that's a parity bug to
   fix in the same change, per the "Don't port Jellyfin bugs / keep Hermit correct
   only on *accepted* divergences" rule — emitting extra fields breaks strict clients
   (see Android TV crash history: strict SDK crashes on shape divergence).
3. Check the single-item path (`get_base_item_dto` → same counts logic around
   `dto_service.rs:1366-1380`) for the same waste and apply the same guard.
4. Do **not** change `get_played_and_total_count_batch` itself — its two grouped joins
   are correct for real folders (Series/Season/Folder/BoxSet/Playlist).

## Verification (all required)
- `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo nextest run --workspace`.
- Coverage gate on touched crates: `cargo llvm-cov nextest -p hermit-core
  --fail-under-lines 80 --summary-only` (compare per-file rows vs a stashed baseline —
  the gate has a known local quirk of exiting 1 even on clean main).
- Parity: response bodies for `/Studios`, `/Genres`, `/Persons`, `/Years`,
  `/Shows/{id}/Seasons` (a real-folder control — UnplayedItemCount must survive there)
  byte-identical to Jellyfin via `benchmark/parity.sh`.
- Perf: `benchmark/run-phase-b.sh` with
  `PHASE_B_ENDPOINTS="studios persons items_series items_mixed"` — studios p50 should
  drop toward the v0.6.1 number (~190 ms under 50 VUs). Report before/after.

## Constraints
- Never create/switch branches; work on the shared HEAD.
- No AI-attribution trailers in commits.
- Tests go in the existing domain-named test files (hermit-api has 33 domain test
  files; extend the by-name/user-data ones, never `batchN`-style files).

## Conflicts
Touches `dto_service.rs` in the same region as Plan 3 (DTO projection pass). Run this
plan **before** Plan 3, not in parallel.
