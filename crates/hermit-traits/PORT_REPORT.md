# hermit-traits — Port Report (INTEGRATE)

Port of the *interfaces* in Jellyfin's `MediaBrowser.Controller` into the DI-seam
trait crate. This is a **definitions crate** (trait bodies land in `hermit-core`,
Wave 6) and is therefore **exempt from the 80% line-coverage gate**; the
`options/` module carries the crate's real, testable logic and its own tests.

## Gate results (INTEGRATE)

All commands run at the workspace root `/home/mango/dev/hermit`.

| Gate | Command | Result |
|------|---------|--------|
| Format apply | `cargo fmt --all` | PASS (exit 0, no changes needed) |
| Format check | `cargo fmt --all --check` | PASS (exit 0) |
| Build | `cargo build --workspace` | PASS (exit 0) |
| Clippy | `cargo clippy --all-targets --all-features -- -D warnings` | PASS (exit 0) |
| Tests | `cargo test --workspace` | PASS (exit 0) — `hermit-traits`: **53 passed, 0 failed** |

No gate failed.

Coverage (INFORMATION ONLY, not a gate — definitions crate is exempt):
`cargo llvm-cov nextest -p hermit-traits --summary-only` →
**89.68% lines** (87.54% region, 57.89% function). The uncovered surface is
trait method signatures (no bodies) and the stub traits; the `options/` logic
modules are 90–100% covered. Even though exempt, the crate clears 80% lines.

## Traits ported per module

**58 traits total**, each `#[async_trait] … : Send + Sync` and object-safe.

| Module | Traits | Count |
|--------|--------|-------|
| `chapters` | ChapterManager | 1 |
| `collections` | CollectionManager, PlaylistManager | 2 |
| `configuration` | ServerConfigurationManager, DisplayPreferencesManager | 2 |
| `devices` | DeviceManager | 1 |
| `drawing` | ImageProcessor, ImageEncoder | 2 |
| `dto` | DtoService | 1 |
| `events` | EventManager, ClientEventLogger | 2 |
| `library` | LibraryManager, UserManager, UserDataManager, UserViewManager, MediaSourceManager, SearchManager, MusicManager, LibraryMonitor, SimilarItemsManager | 9 |
| `media_encoding` | MediaEncoder, TranscodeManager, SubtitleEncoder, AttachmentExtractor | 4 |
| `media_segments` | MediaSegmentManager | 1 |
| `net` | AuthorizationContext, AuthService, WebSocketConnection, WebSocketListener, WebSocketManager | 5 |
| `persistence` | ItemRepository, ItemPersistenceService, ItemCountService, ChapterRepository, MediaStreamRepository, MediaAttachmentRepository, PeopleRepository, KeyframeRepository, LinkedChildrenService, NextUpService, ItemTypeLookup | 11 |
| `providers` | ProviderManager | 1 |
| `security` | AuthenticationManager, QuickConnect | 2 |
| `session` | SessionManager | 1 |
| `subtitles` | SubtitleManager | 1 |
| `system` | SystemManager, ServerApplicationHost, ServerApplicationPaths, PathManager, ExternalDataManager | 5 |
| `trickplay` | TrickplayManager | 1 |
| `tv` | TvSeriesManager | 1 |
| `stubs` (deferred) | ChannelManager, LiveTvManager, LyricManager, PluginManager, SyncPlayManager | 5 |

## Object-safety confirmed

Every one of the **58 traits** has a matching compile-time assertion of the form
`fn _assert_object_safe_<name>(_: &dyn <Trait>) {}`. Counts verified 1:1:
**58 traits ↔ 58 `_assert_object_safe_*` functions**. Because these are ordinary
`fn` items, they are type-checked on every `cargo build`, so a regression to a
non-object-safe signature breaks the build.

## Option types + their tests (`src/options/`)

All **7** option types carry `#[cfg(test)] mod tests`; **28 tests total**, all
passing. Coverage on these logic modules is 90–100%.

| Option type | File | #[test] count | Coverage (lines) |
|-------------|------|---------------|------------------|
| `AuthorizationInfo` | `authorization_info.rs` | 3 | 100.00% |
| `DeleteOptions` | `delete_options.rs` | 3 | 100.00% |
| `DtoOptions` | `dto_options.rs` | 5 | 100.00% |
| `ImageProcessingOptions` / `ImageCollageOptions` | `image_processing_options.rs` | 6 | 94.07% |
| `InternalItemsQuery` / `SourceType` | `internal_items_query.rs` | 6 | 90.65% |
| `InternalPeopleQuery` | `internal_people_query.rs` | 2 | 100.00% |
| `ItemImageInfo` | `item_image_info.rs` | 3 | 100.00% |

Every `src/options/` type has tests. ✔

## Skips / defers (as designed by the port plan)

- **C# `BaseItem`/`Folder`/`Video` OOP hierarchy** — not ported. It is a service
  layer in inheritance disguise; trait signatures use `hermit-db` entities
  (`BaseItemEntity`), `hermit-model` DTOs, and `uuid::Uuid` identities instead.
- **Marker/mixin `IHas*` interfaces** — dropped entirely.
- **`LibraryManager` resolver / path / sort / named-view methods** — omitted;
  they depend on the un-ported `BaseItem` tree and become `hermit-core` free
  functions in Wave 6.
- **Deferred subsystems** (Live TV, channels, SyncPlay, plugins, lyrics) — one
  minimal stub trait each in `src/stubs/`; their sub-strategy interfaces
  (`ILiveTvService`, `IChannel`, `IGroupPlaybackRequest`, `ILyricProvider`, …)
  are omitted per the SKIP list. Trait bodies satisfied later by disabled/stub
  impls in `hermit-core`.

## Types flagged as belonging in `hermit-model`

**None.** Audited every `pub struct`/`pub enum` defined in the crate. All
non-`options` value types (`SearchResult`, `SearchHintInfo`,
`SimilarItemsRecommendation`, `ItemWithCounts`, `MediaStreamQuery`,
`TranscodingJobHandle`, `RequestContext`, `AuthenticationRequest`,
`MetadataRefreshOptions`, etc.) are **service-layer request/result value types** —
query parameters, job handles, and result rows that wrap `hermit-db` entities.
They are correctly in the traits crate, not wire DTOs.

Every actual wire/presentation DTO the traits reference is reused from
`hermit-model` (e.g. `UserItemDataDto`, `SessionInfoDto`, `ChapterInfo`,
`DeviceInfo`, `MediaSegmentDto`, `RemoteSubtitleInfo`, `GroupInfoDto`,
`RemoteLyricInfoDto`) rather than redeclared here — confirmed by the per-module
doc comments and `use hermit_model::…` imports. No type sits in `hermit-traits`
that ought to move to `hermit-model`.

## Honesty statement

All five INTEGRATE gates pass (exit 0). Object-safety is asserted for all 58
traits (1:1). All 7 option types have tests (28 total). Coverage 89.68% lines is
reported for information only and is not gated for this definitions crate.
