# Port Report — `hermit-providers`

Port of Jellyfin's `MediaBrowser.Providers` + `MediaBrowser.XbmcMetadata` +
`MediaBrowser.LocalMetadata` (Wave 5). This report is generated at the INTEGRATE
stage and reflects the code as it stands, including honest deferrals.

## Gate results (this run)

| Command | Result |
| --- | --- |
| `cargo fmt --all` | clean (no changes) |
| `cargo fmt --all --check` | PASS |
| `cargo build --workspace` | PASS |
| `cargo clippy --all-targets --all-features -- -D warnings` | PASS (no warnings) |
| `cargo test --workspace` | PASS |
| `cargo llvm-cov nextest -p hermit-providers --fail-under-lines 80 --summary-only` | PASS — **90.89% lines** (floor 80) |

- Tests in `hermit-providers`: **76** (51 in-crate unit tests + 19 NFO-parser
  integration tests in `tests/xbmc_nfo_parsers.rs` + 6 NFO-saver integration
  tests in `tests/xbmc_nfo_savers.rs`). All 76 pass, 0 skipped.
- Coverage floor: 80% lines. Actual: **90.89% lines / 89.76% regions /
  88.92% functions** across the whole crate.

## What was implemented

### 1. `xbmc/` — XbmcMetadata NFO subsystem (the high-value deliverable)

Port of `MediaBrowser.XbmcMetadata.Parsers` + `...Savers`. The **read/write NFO
round-trip** is the core, test-backed deliverable of this crate.

- `base_parser.rs` — shared `BaseNfoParser` core: title/year/rating/plot/genre/
  studio/tag/country/premiered/runtime/uniqueid/actor/thumb/fanart parsing, the
  `DirectoryService` seam for local-artwork resolution.
- `parsers.rs` — per-kind subclasses: `fetch_movie` / `fetch_episode` /
  `fetch_series` / `fetch_season` and the entry points.
- `xml_reader.rs` — an `XmlReader`-shaped streaming cursor over `quick-xml`.
- `xml_ext.rs` — value/date/provider-id extraction helpers.
- `saver.rs` — `BaseNfoSaver` + per-kind savers (`save_movie` / `save_episode` /
  `save_series` / `save_season`, plus album/artist serialization paths).
- `item.rs` / `config.rs` — `NfoBaseItem` shape and saver config.

**Design choice (parity-honest):** file I/O is kept *out* of parse/serialize.
Callers pass document contents in and get contents out, so fixture tests read
the real Kodi `.nfo` corpus in `tests/data/` and the un-mockable filesystem
access does not inflate (or deflate) the parity numbers.

### 2. `local_images/` — LocalMetadata image discovery

Port of `MediaBrowser.LocalMetadata.Images`. Filename-convention scanners mapping
sibling files to typed `LocalImageInfo`:

- `local_image_provider.rs` — the main convention scanner (poster/folder/fanart/
  logo/banner/thumb/clearlogo/disc/cdart, numbered backdrops, `extrafanart/`).
- `episode_image_provider.rs` — episode's own image + `-thumb`.
- `other_providers.rs` — `CollectionFolderLocalImageProvider` +
  `InternalMetadataFolderImageProvider`.
- `directory_service.rs` — `DirectoryService` trait + `FsDirectoryService`
  (the one real-filesystem seam, exercised against a real temp dir in tests).

### 3. `mediainfo.rs` — ffprobe-backed media-info provider

Port of the **object-safe, pure** subset of `FFProbeVideoInfo`:
`get_media_info` builds a `MediaInfoRequest` from a `VideoProbeInput` and drives
it through a borrowed `&dyn hermit_traits::media_encoding::MediaEncoder` (the
ffprobe seam), plus the dummy-chapter creation logic. The real subprocess I/O
lives in `hermit-mediaencoding` behind the `MediaEncoder` trait, not here.

### 4. `provider_manager.rs` — `ProviderManager` trait impl

`LocalProviderManager` implements `hermit_traits::providers::ProviderManager` as
a dependency-free shell (100% covered): read-only descriptor queries return
empty/default results; operations needing the (not-yet-ported) library store or
network return `ServiceError::Backend` describing the deferral — never a silent
success.

### 5. `container_types.rs` — shared DTOs

`MetadataResult`, `NfoItem`, `ItemInfo`, `PersonInfo`, `LocalImageInfo`,
`FileSystemMetadata`, `RefreshResult`, plus `set_provider_id` / `add_person`
helpers (case-insensitive provider-id replacement).

## Traits satisfied

- `hermit_traits::providers::ProviderManager` — implemented by
  `LocalProviderManager` (`provider_manager.rs:57`).
- `hermit_traits::media_encoding::MediaEncoder` — *consumed* as a borrowed
  `&dyn` seam by `FFProbeVideoInfo::get_media_info`; the production implementor
  lives in `hermit-mediaencoding`.
- `xbmc::base_parser::DirectoryService` / `local_images::DirectoryService` —
  in-crate seams with `FsDirectoryService` production impls + test fakes.

## Parity vs the C# xUnit suite

The upstream Jellyfin tests for these areas are the NFO parser/saver round-trip
fixtures (`Jellyfin.XbmcMetadata.Tests`) and the local-image convention tests.
Those are the behaviors ported and covered here:

- **NFO parse:** movie/episode/series/season fixtures parse into the expected
  `MetadataResult` (title, year, ratings, providerids, cast, artwork). 19
  integration tests + in-crate unit tests mirror the xUnit fixture assertions.
- **NFO save:** round-trip savers reproduce the Kodi document shape (6
  integration tests, e.g. `series_round_trip_american_gods`,
  `movie_fetch_valid_success`).
- **Local images:** convention-table resolution, prefix requirements in mixed
  folders, numbered-backdrop gap termination, `extrafanart/` collection, disc/
  cdart preference — mirror the `LocalMetadata` image-provider tests.

The deferred blocks below have **no** counterpart port, so their upstream xUnit
cases are intentionally out of scope for this wave (see DEFERRED.md).

## Deferrals (honest)

Coverage floor is met at 90.89%, but the following remain uncovered/unported by
design — none are silent gaps:

1. **`mediainfo.rs` — `MediaEncoder` subprocess methods.** The specific uncovered
   lines flagged in the coverage summary are the `FakeEncoder` test-double's
   `unreachable!()` / no-op trait-method stubs inside the `#[cfg(test)]` module
   (`extract_audio_image`, `extract_video_image`, `convert_image`,
   `get_input_argument`, `get_time_parameter`) — not production code. The real
   ffmpeg/ffprobe process I/O lives in `hermit-mediaencoding` behind the
   `MediaEncoder` trait and is not unit-testable from this crate.
2. **`mediainfo.rs` — Blu-ray/DVD disc-source size branch.** Deferred to Wave 6
   (needs the disc examiners + library store).
3. **XbmcMetadata residual error/malformed-XML arms.** A few
   error-branch / malformed-document arms in `base_parser.rs`, `parsers.rs`,
   `xml_reader.rs`, `xml_ext.rs` are not exercised by the current fixture corpus.
   Each of those files is still ≥ 83% line-covered.
4. **`container_types.rs` — a few `Option`-`None` accessor branches** (88.93%).
5. **`xbmc/config.rs` (66.67% lines, 9 lines total)** — a small config-holder;
   the low percentage is a handful of untested accessor lines, not missing
   behavior.
6. **Remote API providers** (TMDB / MusicBrainz / OMDB / AudioDb / ListenBrainz)
   — feature-gated (`tmdb` / `musicbrainz` / `omdb`, all off by default),
   deferred as enrichment (need keys + network, not First-Light).
7. **Full `ProviderManager` refresh orchestration** — image-saving pipeline,
   remote-plugin fan-out, and library-store coupling are deferred; the shell
   returns `ServiceError::Backend` for those ops rather than faking success.

## Per-file coverage (lines)

| File | Line cover |
| --- | --- |
| provider_manager.rs | 100.00% |
| xbmc/item.rs | 100.00% |
| local_images/local_image_provider.rs | 97.31% |
| xbmc/saver.rs | 93.41% |
| mediainfo.rs | 92.08% |
| local_images/directory_service.rs | 91.57% |
| xbmc/base_parser.rs | 90.35% |
| container_types.rs | 88.93% |
| xbmc/xml_reader.rs | 87.05% |
| xbmc/parsers.rs | 85.77% |
| xbmc/xml_ext.rs | 83.64% |
| local_images/episode_image_provider.rs | 84.13% |
| xbmc/mod.rs | 84.15% |
| local_images/item.rs | 81.25% |
| local_images/other_providers.rs | 80.65% |
| xbmc/config.rs | 66.67% |
| **TOTAL** | **90.89%** |
