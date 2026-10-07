# Per-user visibility on item lookups

Investigation completed 2026-10-07 against Ferrofin `60f29281`, on branch
`investigate/per-user-item-visibility`. Implementation remains open. No server
code or production data was changed.

## Finding

Confirmed over real HTTP: knowing a hidden item's ID lets a restricted user read
its full DTO, obtain playback sources, download its media, change favorite and
played state, and delete its database row. Jellyfin returns **404** for those
requests. A private playlist returns **200** on detail and **401** on deletion
in Ferrofin, versus **404** for both in Jellyfin.

The missing operation is Jellyfin's user-aware `GetItemById<T>(id, user)`, which
calls `IsVisibleStandalone` before returning the item. Ferrofin's
[`LibraryManager`](../../crates/ferrofin-traits/src/library.rs) exposes only an
unscoped lookup. Its implementation in
[`library_manager.rs`](../../crates/ferrofin-core/src/library_manager.rs)
at line 1234 rejects a nil ID, then calls `ItemRepository::retrieve_item`.
Resolving a user for DTO or playback preferences does not authorize the item.

The original ignored `brain/plans/PLAN_PER_USER_ITEM_VISIBILITY.md` correctly
identified this gap but needs two corrections:

- Current main's delete is **row-only**. Its claim that files are already deleted
  is stale. The live test removes the hidden row and leaves its media file intact.
  This remains a prerequisite for the planned file-deletion implementation.
- “Every by-id route returns 404” is not Jellyfin's contract. A hidden library
  passed as `/Items?parentId=...` returns **401**. An explicit `/Items?ids=...`
  query returns the hidden movie on **both servers**. Preserve these distinctions.

## Evidence

Source oracle: the repository's `UPSTREAM_TAG`, **v12.0-rc7**, commit
`4910aafa1a8227a65a037d3d2d299a32691e4de3`, inspected in the local Jellyfin checkout.
Live oracle: the available **Jellyfin 12.1.0** binary. These are separate evidence
sources; the runtime tests are not represented as a run of rc7.

Ferrofin was rebuilt from the investigation worktree with:

```sh
CARGO_TARGET_DIR=/home/mango/dev/ferrofin/target RUSTC_WRAPPER= \
  cargo build --locked --offline -p ferrofin-server
```

Build passed. The executable reports package version `0.4.1`; its public API
reports compatibility version `10.11.8`.

Each server received a fresh temporary database, an administrator, a restricted
user, and two libraries containing independently generated one-second videos.
The restricted user could access only `Allowed`, with content deletion enabled
for all libraries. Providers and filesystem watching were disabled. All setup,
policy changes, metadata changes, and requests used HTTP. Both servers were
stopped after the probes. Only synthetic files in their own temporary directories
were eligible for deletion.

| Probe | Ferrofin | Jellyfin 12.1 |
| --- | --- | --- |
| Restricted recursive movie browse | 200, only `Allowed` | 200, only `Allowed` |
| Allowed movie detail | 200 | 200 |
| Hidden movie detail | 200, one media source | 404 |
| Hidden movie POST PlaybackInfo | 200, one media source | 404 |
| Hidden movie Download | 200 | 404 |
| Hidden movie Ancestors / SpecialFeatures / Similar | 200 each | 404 each |
| Hidden movie favorite / played writes | 200 each | 404 each |
| Hidden movie UserData read | 200 | 404 |
| Unrestricted admin detail | 200 | 200 |
| Admin detail targeting restricted `userId` | 200 | 404 |
| Restricted administrator detail | 200 | 404 |
| Allowed movie with blocked tag | 200 | 404 |
| Allowed movie lacking the user's allowed tag | 200 | 404 |
| Allowed movie above parental limit | 200 | 404 |
| API key Download, no user | 200 | 200 |
| API key detail targeting restricted `userId` | 200 | 404 |
| Another user's private playlist detail | 200 | 404 |
| Another user's private playlist deletion | 401 | 404 |
| Hidden movie DELETE | 204 | 404 |
| Admin detail after that DELETE | 404 | 200 |
| Media file after that DELETE | still exists | still exists |
| Explicit hidden movie `ids=` browse | 200, hidden movie returned | 200, hidden movie returned |
| Explicit hidden library `parentId=` browse | 200, hidden movie returned | 401 |

The tag and parental probes independently reset the policy and use the allowed
library. The administrator probe restricts the administrator's own policy;
administration does not bypass standalone visibility. PlaybackInfo demonstrates
source disclosure, not a completed player session.

Reproduction harness: `/tmp/ferrofin-visibility-live.py`. Successful final raw
artifacts: `/tmp/ferrofin-visibility-ferrofin-nbmrsrqo/results.json` and
`/tmp/ferrofin-visibility-jellyfin-6a8gnpk2/results.json`. A normalized comparison
is retained in [per-user-item-visibility-http.csv](per-user-item-visibility-http.csv).
The harness requires Python 3, ffmpeg with libx264, and these server binaries:

```sh
python3 /tmp/ferrofin-visibility-live.py ferrofin \
  /home/mango/dev/ferrofin/target/debug/ferrofin-server
python3 /tmp/ferrofin-visibility-live.py jellyfin \
  /tmp/ferrofin-extras-jellyfin-bin/jellyfin.dll \
  --dotnet /tmp/ferrofin-extras-dotnet/dotnet
```

The initial sandboxed runs could not bind loopback sockets. The successful runs
used approved sandbox escalation. Early Jellyfin harness attempts caught its
temporary startup listener; the completed harness waits for a public response
containing `Version` before seeding.

## Required rule, including exceptions

The pinned upstream implementation is in
[LibraryManager.cs](https://github.com/jellyfin/jellyfin/blob/4910aafa1a8227a65a037d3d2d299a32691e4de3/Emby.Server.Implementations/Library/LibraryManager.cs#L1646)
(overloads at 1646–1671, `ItemIsVisible` at 4004–4016) and
[BaseItem.cs](https://github.com/jellyfin/jellyfin/blob/4910aafa1a8227a65a037d3d2d299a32691e4de3/MediaBrowser.Controller/Entities/BaseItem.cs#L1512).

1. Resolve the typed item. Missing/wrong type returns null. With no user, return
   the item. `UserRootFolder` is explicitly visible. There is no general admin bypass.
2. Check the item's virtual `IsVisible(user)` and every parent's
   `IsVisible(user, skipAllowedTagsCheck: true)`. Skipping allowed tags on parents
   does **not** skip blocked tags or parental ratings.
3. For ordinary items, resolve their collection folders. A top parent with no
   path passes the folder-membership portion. No matching collection folders
   also passes that portion. When there are matches, **any accessible collection
   folder is sufficient**. Nonempty `BlockedMediaFolders` takes precedence over
   `EnableAllFolders` / `EnabledFolders`.
4. Resolve collection membership using parent/owner traversal and collection
   paths / `PhysicalLocations` (`LibraryManager.cs:2757–2804`). Do not equate
   `TopParentId` with a virtual library ID: adopted rows use physical folders;
   Ferrofin-scanned rows can use collection folders directly. Owned extras need
   their owner's membership even without a regular parent.
5. Tags include item, parent, and collection-folder tags, normalized using
   `GetCleanValue`. Blocked tags win. Allowed tags require an intersection except
   for the specified root/view context and overrides (`BaseItem.cs:1904–1969`).
6. Ratings prefer inherited custom ratings, then inherited official ratings;
   evaluate with the preferred metadata country. Respect score/subscore, unknown
   ratings, and `BlockUnratedItems`. Plain folders and name items are exempt from
   the base unrated check; Series, Season, MusicAlbum and BoxSet override it.
7. Preserve kind-specific behavior: `CollectionFolder.IsVisible` checks library
   `Enabled`; `Folder` applies collection-folder policy except plugin folders;
   `Person` skips allowed-tag checks; channel content skips collection membership
   and checks channel access. Channel blocked/enabled preferences have their own
   precedence. A modern shared playlist uses open access, ownership, or explicit
   sharing; a file playlist uses base behavior. Non-legacy BoxSets have linked
   library and child-rating rules. Do not invent a universal Live TV permission
   check here: controller policies and Live TV view availability are distinct.

The existing browse query is **not a substitute**. `hidden_media_folders` only
filters root media folders; `scope_to_user_libraries` deliberately stops adding
library scope when `item_ids` or an explicit parent already exists.
`translate_query::append_parental_rating` now implements rating limits, but user
`AllowedTags`, `BlockedTags`, and `BlockUnratedItems` are stored/read by the user
manager without corresponding complete visibility enforcement. Shared policy
facts can be reused; browse and standalone lookup must retain their different
upstream semantics.

## Call-site inventory and integration points

[The source inventory](per-user-item-visibility-lookups.csv) records all 89
`GetItemById` / `GetParentItem` call sites in the pinned upstream API controllers,
including action, route, exact line, and scoped/unscoped classification. It is a
direct-call inventory, not a claim that delegated manager paths have identical
behavior. Legacy aliases delegate to the actions below and need route tests too.
There are 71 user-scoped calls, 10 raw calls, and 8 raw parent resolutions;
these are call counts, not counts of affected Ferrofin routes. In particular,
upstream `GET /Items/{itemId}/Collections` is absent from Ferrofin's registered
route catalog and is a separate surface gap; it requires a user (401 if absent).

| Upstream controller/actions | Ferrofin integration | Effective user / behavior |
| --- | --- | --- |
| UserLibrary: detail, intros, favorite/rating writes, local trailers, special features | `items.rs`, `user_library.rs` | Resolve target `userId` with the existing permission check; hidden seed → 404 |
| Items: read/write user data | `user_library.rs` | Target user; gate before reading or saving data |
| Library: delete single/batch, file, download | `items.rs`, `library.rs`, `videos.rs` | Caller; no-user API key remains supported where upstream allows it; visibility before CanDelete/CanDownload |
| Library: ancestors, themes, similar | `items.rs`, `library.rs`, `similar.rs` | Effective optional target user; preserve per-action nil/root handling; ThemeMedia delegates to theme actions |
| MediaInfo: GET/POST PlaybackInfo; UniversalAudio | `media_info.rs`, `audio.rs` | Effective optional user upstream; gate before media-source resolution |
| Playstate: mark played/unplayed | `playstate.rs` | Target user; gate before state mutation |
| TV: seasons/episodes | `tv_shows.rs` | Effective optional user; preserve typed Series/Season checks and each query branch |
| InstantMix: songs/albums/artists/items/playlists/genre-ID seeds | `instant_mix.rs` | Effective optional user; typed Playlist seed; name-based routes have different seed resolution |
| Images: info, GET/HEAD variants, mutations | `images.rs` | Caller ID; preserve existing auth policy, including no-user image serving |
| RemoteImage, ItemLookup, ItemRefresh, ItemUpdate | matching handler modules | Caller ID; existing elevation checks still apply; admin is not a visibility bypass |
| Lyrics; subtitle delete/search/download/upload/HLS playlist | `lyrics.rs`, `subtitles.rs` | Caller ID; preserve Audio/Video typing |
| Video attachments; trickplay tile JPEG; media segments | matching handler modules | Caller ID; trickplay gates `mediaSourceId ?? itemId` |
| Videos: additional parts, alternate-source removal, merge | `videos.rs` | Target user for parts; caller for mutations. Merge filters unavailable videos and returns 400 if fewer than two remain |
| LiveTV: channel/recording detail, recording delete | `live_tv.rs` | Optional target user for reads; caller for delete; preserve surrounding TV policies |

Do not implement this as a router middleware keyed on `{itemId}` or solely by
replacing `get_item_by_id` calls:

- PlaybackInfo calls `MediaSourceManager`, which reads the repository directly
  (`media_info.rs:148`, `media_source_manager.rs:1027`).
- Images serve matching image rows without fetching an item; the existence probe
  only runs on a miss (`images.rs:595`). Visibility must precede bytes and cached
  or conditional responses. Preserve the existing no-user behavior.
- Ancestors reads a repository chain, and media segments/trickplay use their
  specialized managers. These paths also need the scoped seed check.
- `SyncPlay::visible_subset` uses `get_item_ids` with explicit IDs
  (`sync_play_manager.rs:1539`), but upstream `Group.HasAccessToQueue` calls
  `IsVisibleStandalone`. It needs a batched standalone check, not the unchanged
  browse query. Its comment claiming ratings are wholly absent is stale.

Exceptions to blanket switching:

- `LibraryManager.GetParentItem` resolves an explicit parent **without** a user.
  `ItemsController.GetItems:342–350` separately checks `IsVisible` and returns
  **401**. Other parent-based query actions must retain their own downstream
  behavior; merely finding `GetParentItem(parentId, userId)` is not proof of a
  scoped lookup.
- `Items?ids=` is a query filter; the live reference returns the hidden item.
  Do not use it as the implementation of standalone authorization.
- General audio/video/HLS streaming uses unscoped `StreamingHelpers` resolution;
  UniversalAudio is explicitly scoped. Subtitle file delivery's helper and
  trickplay's HLS playlist differ from subtitle-HLS and trickplay-JPEG actions.
  Preserve each route's actual checks rather than imposing a new common 404.
- Playlist/collection management delegates to managers with ownership, edit,
  child-filtering and elevation rules. Private playlist lookup through `/Items`
  does not establish the expected status for every `/Playlists` action.
- Session reporting and internal primary-version lookups are not equivalent to
  user-addressed item lookup. Keep internal raw access available.

## Implementation plan

1. Add an explicit user-aware trait operation, for example
   `get_item_by_id_for_user(id, Option<&UserEntity>)`, with an implementation
   shared by API handlers and a batch form for SyncPlay. Keep raw lookup for
   internal and upstream-unscoped callers. Do not give production implementations
   a default that silently falls back to the raw lookup.
2. Implement standalone and per-kind rules in core. Share folder preferences,
   tag normalization, rating helpers, membership and owner/ancestor facts with
   browse and deletion policy code, without treating browse result membership
   as authorization. Keep service/database failures as errors, distinct from an
   invisible item (`None`). A per-request context can load user preferences and
   library facts once; do not cache visibility globally by item ID alone.
3. Wire the scoped action families above, retaining their user selection,
   API-key/null-user behavior, type checks, nil-ID root handling, and permission
   order. Gate before DTO generation, media access, state writes and deletion.
   Batch deletion remains ordered and stops at its first invisible item; earlier
   successful deletions remain committed. Preserve the existing cancellation
   handling.
4. Correct `/Items?parentId=` with its own `IsVisible` → 401 gate. Add the
   standalone batch check to SyncPlay. Audit delegated playlist/collection and
   session paths against their manager behavior before changing those actions.
5. Add tests with real managers/repositories, not only stubs that implement raw
   lookup. Cover each switched HTTP action and alias with an explicit expected
   result. An operation matrix should distinguish scoped 404, parent 401,
   merge filtering/400, user-less access, and intentionally unscoped queries.

Required rule fixtures: enabled/blocked library precedence; shared physical
locations; adopted versus newly scanned hierarchy; owned extras; hidden parent;
normalized/inherited tags and blocked-tag precedence; score/subscore and unrated
kind overrides; missing/wrong-type/nil IDs; root/name/view items; disabled library;
channel preferences; public/private/shared/file playlists; legacy/modern BoxSets;
admin and API-key with/without explicit target user; policy change after cache
warm-up; batch delete stopping order; image cache hits and conditional requests.

Run the reproduction against the implementation, extend it to images, channels,
Live TV, BoxSets, inherited rules and SyncPlay, then run normal format, clippy,
test and coverage gates. The current live matrix establishes the reported gap,
not complete route coverage or all per-kind rules. No standalone visibility
unit-test matrix was found in the pinned upstream test tree by the symbol search;
the C# implementations provide the oracle for new cases.

Measure detail, PlaybackInfo, image hits, and SyncPlay batch access before/after
using the same data and warm/cold policy state. Pay particular attention to
per-parent queries and repeated user-preference reads. No performance comparison
is claimed for this investigation: production code is unchanged. No schema
migration is expected, but tests must include adopted physical-folder layouts.
