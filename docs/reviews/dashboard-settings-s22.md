# S22: a video being recorded is keyed under Live TV

`Video.SourceType` is `LiveTV` while `RecordingsManager.GetActiveRecordingInfo(Path)`
knows the file (`Video.cs:171–181, 454–457`). `BaseItem.SourceType` is `Channel`
for any other item with a ChannelId, and only `Channel` items put their
`ExternalId` ahead of their own id in `GetUserDataKeys` (`BaseItem.cs:1733–1747`).
So a recording that carries a ChannelId and ExternalId is keyed by its channel
id *until* its file is being written to, and by its own id (the Live TV arm)
while it is. Ferrofin's in-memory folder filter chose the channel key from the
row alone, so favorites, likes, resume and played filters over a physical
folder's children read the channel-keyed row of a recording that Jellyfin reads
by the item's own key.

Source oracle: Jellyfin `4910aafa1a` (`Video.SourceType`, `Video.IsActiveRecording`,
`BaseItem.SourceType`, `BaseItem.GetUserDataKeys`); evidence in the living
review row S22.

## Behavior

- `FerrofinItemRepository::folder_own_user_data` treats a video whose `Path`
  is among Live TV's active-recording paths as `LiveTV`: no channel
  `ExternalId` key is inserted for it. Non-video kinds, videos that are not
  recording, and recordings without a ChannelId/ExternalId are unchanged.
- The fact comes from the real Live TV manager (`active_recording_paths`, the
  same source delete/trickplay/subtitle checks use), not from the stored `Data`
  blob or the row's own fields. The repository cannot own the manager (Live TV
  is built after it and needs the DTO service), so it holds a late-bound slot
  (`ActiveRecordingSlot`) that the composition root fills once with a closure
  over a **weak** manager reference: no ownership cycle, and a dropped manager
  reads as "nothing recording". The manager is asked only when a page contains
  a video that could be channel-keyed, so ordinary browses add no work.

## Verification

- Core test: four items with the same channel external key and a favorite
  stored only under that key — an active recording movie, an inactive movie, an
  active episode and a folder at the same path. Before the slot is bound all
  four select by the channel key; once Live TV reports `/rec/active.ts`, the two
  active videos drop out and the inactive movie and the folder keep the channel
  key.
- Production binary, one M3U channel, a manual timer recording into a library
  folder (the capture is in progress, status `InProgress`), the recording's row
  given a ChannelId and ExternalId with a favorite stored only under that
  external key, user data of the own id not favorite:

  | favorites of the physical folder's children | count |
  |---|---|
  | before (old binary) | 1 (channel key read) |
  | after (new binary) | **0** (Live TV key, own row not favorite) |

  Unchanged in both builds: the item's own DTO (`IsFavorite` false), and the
  SQL-translated recursive favorite queries (1 in both).

## Limitations

- The SQL-translated recursive/paged queries and the user-data sorts do not
  consult the active-recording fact; they read the stored key list and are not
  the in-memory GetResult consumer this finding covers. Unifying them with
  `folder_own_user_data` is a separate source comparison.
- `user_data_keys` still never emits a channel key when saving (the known
  omission documented in that module); this change does not add channel items.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,460, extensions 96, livetv 299, wasm
62, server 237. Real FFmpeg tests enabled; the WASM guest build remains
disabled. Line coverage over core's own sources: **95.13%**, above the 80% gate;
the server composition root and traits are coverage-exempt (the slot wiring is
exercised by the live run above). The instrumented build was cleaned afterward
(`target` about 25 GiB, about 494 GiB free).
