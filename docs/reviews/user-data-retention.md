# User data retention

Ferrofin now retains user data when media is removed and recovers it when a
matching item is discovered again. This preserves watched state, play count,
resume position, favorites, likes, ratings, last-played dates, and audio and
subtitle selections. Jellyfin-owned tables and playback keys stay compatible.
Additive migration `0035` introduces Ferrofin-owned source snapshots and their
identity/alias index to make recovery safe across repeated moves and collisions.

Implemented on `fix/user-data-retention`, starting from `60f29281`, in
the seven requested work items and three follow-up edge-case fixes. The original investigation reproduced three
failures: deletion erased history, reattachment matched the wrong key, and
reattachment left the retention timestamp set. Those regressions now run by
default and pass.

## Deletion and recovery

`FerrofinItemPersistenceService::delete_items` preserves history for the entire
removal set, including descendants and owned extras, before deleting media
rows. History moves to the existing placeholder item
`00000000-0000-0000-0000-000000000001` with a retention timestamp. Detachment,
link cleanup, and item deletion share a transaction; a failure rolls back all
three. An authoritative snapshot also retains the source identity and every
compatible alias, without a foreign key back to the deleted media item. Its
user foreign key cascades on user deletion.

When several removed items share a `(UserId, CustomDataKey)`, the latest
`LastPlayedDate` wins, followed by the largest `PlayCount`, then the lowest item
ID for a deterministic tie. Ranking spans the full deletion set, including
sets larger than the SQL bind chunk. Incoming history replaces a conflicting
placeholder snapshot, following Jellyfin's deletion policy. These rules apply
to the compatible mirror; independent source snapshots preserve colliding
histories even when only one can occupy the mirror's bare key.

Playback writes and recovery share `user_data_key_repository`. Recovery uses
identities qualified by media type and provider, including movie provider IDs,
series-derived season and episode keys, and audio metadata. The same numeric
TMDB and TVDB values are distinct recovery identities. It restores affected users' data
and clears the retention timestamp. The selected snapshot is written under
all current keys, including the destination GUID, keeping individual and batch
DTO reads consistent.

Existing destination state wins for each user as a whole, including explicit
unplayed or unfavorite changes. A skipped user's snapshot remains available
for another matching destination, including one on a later recovery page.
For an eligible user, the most recently detached matching snapshot wins
(last-played date, play count, and source ID break batch ties). Consuming a snapshot removes all
its aliases, including an old path's GUID, so moving back cannot revive stale
watched state. Recovery and snapshot consumption share a transaction.

## Scan and refresh integration

Recovery runs after scanner and provider refreshes persist item metadata and
provider IDs. A second recovery pass runs after scan pruning and closing passes,
before the final library-change notification. It also runs on unchanged scans,
so a prior interrupted recovery can be retried.

The completion pass considers persisted destinations outside the current scan
scope. This handles both a new path saved before its old path is pruned and a
destination indexed by an earlier watcher event. Music's closing pass has
finished before this recovery, so its final metadata is available for keys.

The pass processes 500 items per transaction, releases the writer between
pages, and loads identity fields, providers, and series context in batches.
Large item data blobs are not loaded. Only matching candidates perform recovery
writes. Candidate retained keys are read once per pass and kept for subsequent
destinations because earlier ones may skip some users. With no detached history, the pass exits before reading
library items. History detached concurrently after the candidate snapshot is
eligible for recovery on the next scan or destination refresh.

## Other deletion paths

| Path | History handling |
| --- | --- |
| Library pruning and explicit item deletion | Detach the whole deletion set through the persistence service. |
| Orphaned extras during adoption | Use the same service, including descendants and owned extras. |
| Duplicate paths during adoption | Move the duplicate's history to its known survivor; detach history on descendants that will cascade-delete. |
| Container ID migration and localized view consolidation | Move history directly to the known survivor and translate its GUID key; preserve existing destination state. |
| Orphaned person cleanup | Use the same retention-aware persistence deletion. |
| Existing person and artist merges | Already transfer user data before deleting duplicate items; their existing conflict handling is retained. |
| Schema table rebuilds | Already disable foreign-key cascades during migration and validate integrity afterward. Historical migrations are unchanged. |

User deletion still removes that user's history. The user-data cleanup task
still deletes expired detached history, source snapshots, and their alias
indexes, and leaves attached, recent, and undated legacy rows alone.

## Jellyfin compatibility and limits

The pinned Jellyfin 12.0 implementation at
`6c073e19ddf604b2369c638716164fdab4c952dc` detaches history before deletion and
recovers it using `GetUserDataKeys()` in
[ItemPersistenceService.cs](https://github.com/jellyfin/jellyfin/blob/6c073e19ddf604b2369c638716164fdab4c952dc/Jellyfin.Server.Implementations/Item/ItemPersistenceService.cs).
Ferrofin additionally records source snapshots in its own additive tables,
normalizes recovered state across current keys, and preserves existing
destination state. Bare placeholder rows remain available to Jellyfin, but
Jellyfin does not use Ferrofin's extra identity and alias information.

Jellyfin's
[CleanupUserDataTask](https://github.com/jellyfin/jellyfin/blob/6c073e19ddf604b2369c638716164fdab4c952dc/Emby.Server.Implementations/ScheduledTasks/Tasks/CleanupUserDataTask.cs)
removes detached rows older than 90 days when executed and declares no default
triggers. Ferrofin retains its existing 90-day cleanup behavior; this change
adds no cleanup schedule.

Legacy detached rows have no source/provider provenance. Automatic recovery
of those rows requires an exact GUID match; ambiguous provider-only rows stay
detached until cleanup. The migration cannot infer the missing provenance.

Recovery requires a shared identity key. A file restored at its original path
can match its GUID; an unidentified file moved to a new path cannot be matched
reliably if its only key was the old path-derived GUID. Unmatched history stays
detached until eligible cleanup. History already deleted before this fix needs
a pre-loss backup or another external source.

## Commits

1. `8cadd5b9` — detach history before deleting media.
2. `e270ee29` — resolve detached-history key collisions.
3. `48fbde73` — share stored user-data key resolution.
4. `59ac214d` — restore history using shared identity keys.
5. `1b4ac837` — recover history after refresh and scan pruning.
6. `a38aa772` — preserve history in repairs and orphan cleanup.
7. `8b807674` — extend regression coverage and document behavior.
8. `92277742` — consume complete source snapshots on recovery.
9. `690e2d7c` — retain skipped users' snapshots for other destinations.
10. Identity-scoping follow-up — distinguish media types and providers during recovery.

## Regression coverage

The tests exercise deletion and original-path restoration; movie, episode,
season, audio, and audiobook identities; every persisted history field;
multiple users; destination-state conflicts; repeated and concurrent recovery;
deterministic duplicate selection; deletion batches and recovery scans larger
than one page; unknown keys; transaction rollback; container and view ID
migration; orphan cleanup; and expired versus attached retention data.

`library_scan_watcher` uses actual temporary files and directories with a local
metadata fixture. Its four retention cases cover a move in one scan,
destination-first and source-first scans, and restoring the original path.
The original four include an unchanged rescan after recovery. Two additional
filesystem cases move a file back to its original path after changing watched
state, with source-first and destination-first discovery.

Service regressions also cover two users and duplicate destinations on the
same/different pages; simultaneous retained movie/series snapshots with equal
numeric provider IDs; equal IDs from different providers on the same media
type; conservative legacy handling; and expiration/user deletion of snapshots.

The shared user-data tests compare individual-item and batch DTOs after
recovery and after a subsequent state change. These are service-level DTO
checks, not browser interaction tests.

Provider refresh tests record the provider IDs visible at the recovery call,
verifying that recovery follows metadata and identity writes. The scan tracing
smoke test waits for span export separately from queue idleness, since SQLite's
worker thread can briefly retain a span after returning a query result.

Validation commands use `RUSTC_WRAPPER=` and the shared `CARGO_TARGET_DIR`:

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features --offline -- -D warnings
cargo nextest run --workspace --offline --no-fail-fast
cargo test --workspace --doc --offline
```

Validation of the follow-up changes passed:

- Full workspace: 7,580 tests passed, 5 skipped, including all six filesystem
  retention scenarios and the scan tracing smoke test.
- Database suite: 121 tests passed, 1 skipped, including migration checksums,
  adoption, schema conformance, and the SQL boundary check.
- After the final recovery tie-break refinement: all 33 selected retention
  tests passed, including both filesystem move-back cases.
- All 3 doctests, formatting, and Clippy across all targets/features with
  warnings denied passed.
