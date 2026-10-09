# Per-user visibility on item lookups

Investigated and implemented on branch `investigate/per-user-item-visibility`,
starting from Ferrofin `60f29281`. User-scoped by-ID actions now evaluate standalone
visibility before returning data or changing state. The 33 live HTTP observations
match Jellyfin 12.1.0, including the intentional exceptions described below.
All runtime fixtures use disposable databases and synthetic media.

## Finding

Confirmed over real HTTP on the baseline: knowing a hidden item's ID let a restricted user read
its full DTO, obtain playback sources, download its media, change favorite and
played state, and delete its database row. Jellyfin returns **404** for those
requests. A private playlist returns **200** on detail and **401** on deletion
in Ferrofin, versus **404** for both in Jellyfin.

The missing operation was Jellyfin's user-aware `GetItemById<T>(id, user)`, which
calls `IsVisibleStandalone` before returning the item. Ferrofin's
[`LibraryManager`](../../crates/ferrofin-traits/src/library.rs) previously exposed only an
unscoped lookup. Its baseline implementation in
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

## Baseline evidence

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

The browse query is **not a substitute**. `hidden_media_folders` only
filters root media folders; `scope_to_user_libraries` deliberately stops adding
library scope when `item_ids` or an explicit parent already exists.
`translate_query::append_parental_rating` implements rating limits, but on the
baseline the user manager stored/read `AllowedTags`, `BlockedTags`, and
`BlockUnratedItems` without corresponding complete visibility enforcement.
Browse and standalone lookup retain their different upstream semantics.

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

The baseline also had indirect lookups that required explicit gates; router
middleware keyed on `{itemId}` or replacing `get_item_by_id` alone would miss them:

- PlaybackInfo called `MediaSourceManager`, which reads the repository directly
  (`media_info.rs:148`, `media_source_manager.rs:1027`).
- Images served matching image rows without fetching an item; the existence probe
  only ran on a miss (`images.rs:595`). Visibility now precedes bytes and cached
  or conditional responses, preserving the existing no-user behavior.
- Ancestors read a repository chain, and media segments/trickplay use their
  specialized managers. These paths now have scoped seed checks.
- `SyncPlay::visible_subset` used `get_item_ids` with explicit IDs
  (`sync_play_manager.rs:1539`), but upstream `Group.HasAccessToQueue` calls
  `IsVisibleStandalone`. It now uses the batched standalone check. The old
  comment claiming ratings were wholly absent was also stale.

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

## Implementation, in commit order

| Item | Work completed | Commit |
| --- | --- | --- |
| 1 | Explicit user-scoped lookup, standalone predicate and batch contracts; retain raw lookup and fail closed when visibility is unavailable | `9f027d19` |
| 2 | Request-local policy evaluation, hierarchy/owner membership, inherited tags and ratings, playlist/BoxSet/channel rules, adopted paths | `6d1a34a2` |
| 3 | Direct read/write, playback, metadata, TV, lyrics/subtitles and version actions; retain effective-user and type semantics | `2aecb4d6` |
| 4 | Images before cache/304 responses, ancestors, attachments, media segments and trickplay tiles | `4df593fb` |
| 5 | Visibility before deletion permission; ordered batch stop and cancellation behavior | `40eeb440` |
| 6 | Batched standalone SyncPlay queue access, including policy changes | `0c86e327` |
| 7 | Parent-browse 401, explicit-ID browse and user-less/raw lookup exceptions | `bcdb39f2` |
| 8 | Final regression checks, SQL repository boundary, Live TV source-type correction, live parity and performance evidence | Final validation commit |

The evaluator loads policy and reachable hierarchy once per request or batch.
Database reads live in `item_visibility_repository.rs`; service/database failures
remain errors rather than being converted to invisible items. There is no global
item-only visibility cache and no schema migration.

The real-manager HTTP tests in
[`apps/ferrofin-server/tests/item_visibility.rs`](../../apps/ferrofin-server/tests/item_visibility.rs)
cover direct reads/writes, aliases, cached images, user data, typed seeds,
restricted administrators, explicit target users, indirect media reads,
deletion ordering and intentional exceptions. Core fixtures exercise adopted
physical folders, shared library locations, owner extras, inherited normalized
tags, custom/official ratings and subscore, unrated kind overrides, disabled
libraries, playlist sharing, nested BoxSets, channel preferences and corruption.
Live TV guide `ChannelId` does not imply a plugin-channel access check, matching
the upstream source-type override. SyncPlay includes a 1,200-ID batch fixture.

The retained [fixed HTTP matrix](per-user-item-visibility-fixed-http.csv) records
33 matching observations, including image GET/HEAD/conditional responses and
SyncPlay group list/detail. This matrix complements the route and policy tests;
it does not claim live coverage of every item kind or controller action.

The reproducible harness is now checked in:

```sh
RUSTC_WRAPPER= cargo build --locked --offline -p ferrofin-server
python3 bench/per_user_visibility.py jellyfin /path/to/jellyfin.dll \
  --dotnet /path/to/dotnet
python3 bench/per_user_visibility.py ferrofin target/debug/ferrofin-server \
  --reference /tmp/ferrofin-visibility-jellyfin-EXAMPLE/results.json
```

Each invocation prints its temporary artifact directory and stops its own server.
Use a worktree-local Cargo target directory to avoid artifacts from other branches.
Deletion still removes database rows only; physical-file deletion and the absent
`/Items/{itemId}/Collections` route remain separate work.

## Validation

| Check | Result |
| --- | --- |
| `cargo fmt --all --check` and `git diff --check` | Passed |
| `cargo clippy --locked --offline --all-targets --all-features -- -D warnings` | Passed |
| `cargo nextest run --locked --offline --workspace --no-fail-fast` | 7,575 passed, 5 skipped |
| `cargo test --locked --offline --workspace --doc` | 3 passed |
| Final binary against the saved Jellyfin 12.1 observations | All 33 matched |

Cargo commands used `RUSTC_WRAPPER=` and an isolated worktree target directory.
The workspace run includes the SQL-boundary, schema, API-contract and real HTTP
integration tests. Its first run caught SQL in the visibility evaluator; moving
the reads into the repository module resolved the failure without raising the
architecture test's ceilings.

| Per-crate line coverage | Covered / total lines | Result |
| --- | --- | --- |
| `ferrofin-core` | 84,482 / 89,512 | 94.38% |
| `ferrofin-api` | 16,384 / 19,007 | 86.20% |
| `ferrofin-livetv` | 10,118 / 10,793 | 93.75% |

Each changed, non-exempt crate clears the 80% gate. These figures exclude other
workspace crates and apps using the filename filter specified in
`CONTRIBUTING.md`. The initial unfiltered API/Live TV aggregate included uncovered
dependencies and was unsuitable for the per-crate gate; the filtered reports
above are the final results. Core and API each passed a fresh instrumented
nextest run with the filter; Live TV's report uses its completed 296-test run.

## Before/after timing

Measured the saved `60f29281` server executable and the final implementation with
the same harness and unoptimized debug profile. Each invocation created a fresh
database with 64 movies in two libraries; the allowed SyncPlay queue contained
63 movies. All four probes used GET. After functional probes, each route received
10 warm-up requests and 100 sequential timed requests. Trial order was before,
after, after, before, before, after; our compilation and coverage jobs had finished.

```sh
python3 bench/per_user_visibility.py ferrofin /path/to/before/ferrofin-server \
  --benchmark 100 --library-size 64
python3 bench/per_user_visibility.py ferrofin target/debug/ferrofin-server \
  --benchmark 100 --library-size 64
```

The table takes the median of each build's three trial percentiles, in milliseconds.
The [measurement artifact](per-user-item-visibility-performance.json) retains
all six trial summaries, sample counts, first samples and host load observations.

| Route | Before p50 | After p50 | Change in p50 | Before p95 | After p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Item detail | 2.85 | 3.63 | +0.79 | 5.50 | 6.28 |
| PlaybackInfo | 1.26 | 2.67 | +1.40 | 1.90 | 4.42 |
| Cached image | 0.84 | 1.94 | +1.10 | 1.13 | 3.22 |
| SyncPlay list, 63 queue items | 1.47 | 7.56 | +6.09 | 2.21 | 10.78 |

The new checks add measurable work. SyncPlay has the largest observed increase
(about 5.1× p50), because its previous explicit-ID browse shortcut did not evaluate
standalone visibility. It now loads the queue entities and policy/hierarchy facts
in batches and evaluates every item. The other observed p50 increases are below
1.5 ms. These are limited warm-loopback measurements on a shared host: one-minute
load averages ranged from 35.9 to 91.1 at trial starts. They are not production,
cold-cache, concurrency, or throughput results, and no latency SLO is claimed.
The artifact's first sample follows the functional probes and is not a cold-cache
measurement. Immediate policy changes are covered by correctness tests.
