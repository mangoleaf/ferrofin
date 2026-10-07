# User data retention investigation

Confirmed at Ferrofin `60f29281`: removing a media item destroys its associated
watch history, resume position, favorites, ratings, and stream preferences.
Moving a file normally changes its item ID, so pruning the old path loses the
same data. The database already contains the structures needed for Jellyfin's
retention model; the missing pieces are in the deletion and recovery lifecycle.

This investigation is on `investigate/user-data-retention`. Production behavior
is unchanged. The accompanying ignored regression tests express the required
behavior and can be run explicitly while implementing the fix.

## Where history is lost

1. `LibraryScanner::prune_deleted` calls `delete_items` for missing paths,
   including deleted, renamed, and moved files
   (`crates/ferrofin-core/src/library_scan.rs:6184–6320`). Unreachable library
   roots and failed directory listings have separate guards against pruning.
   The library manager's explicit deletion path also calls this service
   (`library_manager.rs:1574–1620`).
2. `FerrofinItemPersistenceService::delete_items` computes the full deletion
   set, including descendants and owned extras, removes container links, then
   deletes `BaseItems` in one transaction. It never detaches `UserData`
   (`item_persistence_service.rs:1425–1530`).
3. `UserData.ItemId` references `BaseItems.Id` with `ON DELETE CASCADE`
   (`crates/ferrofin-db/migrations/0001_initial.sql:619–637`). Runtime connections
   enable foreign keys (`crates/ferrofin-db/src/database.rs:113`). Consequently
   every user-data key on each deleted item is physically removed. Provider
   keys do not protect those rows from the cascade.

Item IDs derive from the item kind and filesystem path
(`item_type_lookup.rs:93–118`). Restoring the original path can restore the ID,
but it cannot restore deleted rows. A new path generally produces a different
ID; recovery then needs a stable user-data key such as a movie provider ID or
the series key plus season and episode numbers.

## Recovery is also incomplete

`reattach_user_data` exists at `item_persistence_service.rs:2812–2832`, but:

- It matches only `PresentationUniqueKey`, instead of the complete set returned
  by the existing `user_data_keys` derivation. A normal movie presentation key
  is a GUID without hyphens, whereas its GUID user-data key has hyphens. Movie
  provider IDs and episode keys also need their own derivation.
- It updates `ItemId` without clearing `RetentionDate`.
- A repository-wide search finds no production call to it. Apart from its
  implementation, the existing references are a trait declaration and a mock.
  Neither scanner saves nor provider refreshes invoke it.

Both individual reads and batch DTO reads restrict history to the current
`ItemId` (`user_data_manager.rs:167–169,1085–1087`). Therefore even retained rows
imported from Jellyfin remain invisible until they are properly reattached.
The current timestamp bug alone does not make attached rows expire: cleanup
also requires the placeholder item ID. Clearing it still matters for parity
and a correct lifecycle.

## Jellyfin behavior

The local Jellyfin 12.0 checkout at
`6c073e19ddf604b2369c638716164fdab4c952dc` implements this sequence in
[ItemPersistenceService.cs](https://github.com/jellyfin/jellyfin/blob/6c073e19ddf604b2369c638716164fdab4c952dc/Jellyfin.Server.Implementations/Item/ItemPersistenceService.cs#L50):

1. Before deletion, move user data onto the placeholder item
   `00000000-0000-0000-0000-000000000001` and set its retention timestamp.
2. Resolve collisions on `(UserId, CustomDataKey)` before changing `ItemId`.
   For duplicate keys within the deletion batch, prefer the latest
   `LastPlayedDate`, then the largest `PlayCount`. Incoming history replaces
   conflicting placeholder rows.
3. Reattach placeholder rows matching any `GetUserDataKeys()` entry and clear
   their retention timestamps. This operates across users, retaining user
   identity on each row.

[MetadataService.SaveItemAsync](https://github.com/jellyfin/jellyfin/blob/6c073e19ddf604b2369c638716164fdab4c952dc/MediaBrowser.Providers/Manager/MetadataService.cs#L308)
invokes recovery after saving on the first refresh. The local 10.11.8 checkout
also detaches and reattaches history in `BaseItemRepository.cs:108–139,773–805`.

Jellyfin's
[CleanupUserDataTask](https://github.com/jellyfin/jellyfin/blob/6c073e19ddf604b2369c638716164fdab4c952dc/Emby.Server.Implementations/ScheduledTasks/Tasks/CleanupUserDataTask.cs#L49)
deletes placeholder rows older than 90 days when the task runs; it declares no
default triggers. This is retention with later cleanup, not a promise of
permanent history. Recovery also requires a matching key: moving unidentified
media whose only key is its path-derived GUID cannot guarantee recovery.

Ferrofin already has the placeholder, `CustomDataKey`, `RetentionDate`, key
derivation, and a 90-day cleanup task
(`scheduled_tasks/maintenance.rs:635–688`). No schema change appears necessary
for the basic retention lifecycle. The cleanup task's comments currently claim
that deletion parks rows on the placeholder, which the implementation does not do.

## Recommended implementation

Preserve user data for the entire deletion set before deleting any `BaseItems`,
within the existing transaction. Resolve duplicate keys across the whole set,
including across SQL chunks, and existing placeholder conflicts. Retain the
foreign keys and normal media-row deletion semantics.

Share the full key resolution currently used by `FerrofinUserDataManager`
with recovery. This includes provider IDs, series context for episodes and
seasons, and audio metadata. Reattach all matching users' rows, clear retention,
and define how to handle a key already attached to the destination so a retry
or concurrent playback write cannot cause a uniqueness failure or silently
overwrite newer state.

Wire recovery after item metadata and provider IDs are persisted. Also handle
same-scan moves: the scanner processes new items before `prune_after_scan`
(`library_scan.rs:4290–4333`), so the old history may not be detached during
the new item's first save. A recovery pass after pruning, including already
saved destinations affected by the deletion, is one approach. Cover watcher
events arriving in either order and retries after interrupted scans.

Audit direct production `BaseItems` deletions outside the persistence service,
including adoption repairs, for whether they should preserve user data. Keep
intentional user deletion and retention cleanup distinct from media removal.
Already cascaded-away rows need a pre-loss backup or other external history
source; adding retention cannot reconstruct them.

## Reproduction and acceptance checks

The three `retention_investigation` cases in
`crates/ferrofin-core/src/item_persistence_service.rs` use the real persistence
service and a migrated in-memory database. They are ignored by default because
this branch investigates the defect without implementing its fix. Run:

```sh
cargo test -p ferrofin-core --lib --offline retention_investigation -- --ignored
```

They require history to survive deletion on the placeholder, GUID-keyed history
to recover despite a different presentation key, and successful reattachment
to clear the retention timestamp. The timestamp case deliberately aligns the
presentation key with the history key to isolate that defect.

Explicit execution produced three failures, each at its intended assertion:

| Case | Observed result |
| --- | --- |
| Delete preserves history | Zero rows remained; one retained row was expected. |
| Reattach by user-data key | The row still belonged to the placeholder. |
| Clear retention timestamp | The row moved to the item but retained its timestamp. |

Compilation succeeded and the three cases completed in 0.13 seconds. Their
failure is the reproduction result, not a passing retention implementation.
The normal persistence test run passed all 62 existing tests, with the three
new cases ignored. `cargo fmt --all --check` and `git diff --check` passed.

The implementation should additionally cover:

- Delete and restore at the same path, preserving every user-data field for
  multiple users.
- Movie moves with provider IDs, episode and season moves with series keys,
  and audio moves with stable metadata; verify both individual and batch DTOs.
- A destination discovered before the source is pruned, including watcher
  scans and an already indexed destination.
- Duplicate provider keys within a deletion batch, preexisting placeholder
  conflicts, existing destination history, and repeats after recovery.
- Descendants and owned extras, deletion sets larger than the bind chunk,
  and rollback on a failure after detachment.
- Recovery from a Jellyfin-created detached row, expired placeholder cleanup,
  and preservation of attached or unexpired rows.

These service-level regressions do not constitute an end-to-end filesystem or
HTTP reproduction. The scan and API reachability above comes from source tracing.
