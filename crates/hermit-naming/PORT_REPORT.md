# hermit-naming — Port Report

Port of Jellyfin's **`Emby.Naming`** C# library (filename → media parsing) to Rust.
Upstream reference: `~/dev/3rdparty/jellyfin/Emby.Naming` (source) and
`~/dev/3rdparty/jellyfin/tests/Jellyfin.Naming.Tests` (xUnit suite).

## Integration gate — all six checks PASS

| # | Command | Result |
|---|---------|--------|
| 1 | `cargo fmt --all` | PASS (no changes) |
| 2 | `cargo fmt --all --check` | PASS (clean) |
| 3 | `cargo build --workspace` | PASS |
| 4 | `cargo clippy --all-targets --all-features -- -D warnings` | PASS (no warnings) |
| 5 | `cargo test --workspace` | PASS |
| 6 | `cargo llvm-cov nextest -p hermit-naming --fail-under-lines 80` | PASS — **96.49%** line coverage |

## Coverage

- **Line coverage: 96.49%** (76 uncovered of 2164 lines) — well above the 80% gate.
- Function coverage: 97.09% · Region coverage: 96.75%.
- Tests: **684 test invocations** run across 29 nextest binaries, all green.

Lowest-covered files (still all ≥ 87%): `video/clean_date_time_parser.rs` 87.50%,
`video/file_stack.rs` 93.33%, `video/clean_string_parser.rs`/`video/file_stack_rule.rs` ~93–96%,
`video/video_list_resolver.rs` 96.09%, `tv/season_path_parser.rs` 96.95%. Uncovered lines are
defensive/unreachable branches (e.g. regex that always matches after a pre-check), not missing behavior.

## Public types (by module)

- **common**: `MediaType` (enum), `EpisodeExpression`, `NamingOptions` — regex/config tables copied byte-for-byte from C#.
- **audio**: `AlbumParser`, `AudioFileParser`.
- **audiobook**: `AudioBookFileInfo`, `AudioBookInfo`, `AudioBookFilePathParser`(+`Result`), `AudioBookNameParser`(+`Result`), `AudioBookResolver`, `AudioBookListResolver`.
- **book**: `BookFileNameParser`, `BookFileNameParserResult`.
- **tv**: `EpisodeInfo`, `SeriesInfo`, `EpisodePathParser`(+`Result`), `EpisodeResolver`, `SeasonPathParser`(+`Result`), `SeriesPathParser`(+`Result`), `SeriesResolver`, `TvParserHelpers`.
- **video**: `VideoFileInfo`, `VideoInfo`, `VideoResolver`, `VideoListResolver`, `CleanDateTimeParser`(+`Result`), `CleanStringParser`, `ExtraResult`/`ExtraRule`/`ExtraRuleType`/`ExtraRuleResolver`, `Format3DParser`/`Format3DResult`/`Format3DRule`, `FileStack`/`FileStackRule`/`StackResolver`, `StubResolver`/`StubTypeRule`, `NumericOrdering`.
- **external_files**: `ExternalPathParser`(+`Result`), `LocalizationManager` seam.
- **io** / **path**: `FileSystemMetadata` POCO + path helpers (the BCL/`IFileSystem` seam that C# gets for free).

Real dependency types (`ExtraType`, `SeriesStatus`, `DlnaProfileType`, `CollectionType`, `CultureDto`) are
reused from `hermit-model` rather than re-stubbed.

## Parity vs the C# xUnit cases

Source and test files map **1:1** onto the C# tree. C# test project totals:

- **125 `[Fact]`** + **33 `[Theory]`** methods, expanding via **560 `[InlineData]`** rows + 2 `[MemberData]` sources.
- Effective C# test invocations ≈ 685 (125 Facts + ~560 parameterized rows).

Rust port: **684 rstest invocations**, with one binary per C# test file
(`tv_daily_episode`, `tv_absolute_episode_number`, `tv_episode_number`,
`tv_episode_number_without_season`, `tv_multi_episode`, `tv_season_number`,
`tv_season_path_parser`, `tv_series_path_parser`, `tv_series_resolver`, `tv_simple_episode`,
`tv_episode_path_parser`, `tv_parser_helpers`, `music_multi_disc_album`, `book_resolver`,
`external_path_parser`, `audiobook_*`, `video_*`, `common_naming_options`, …). Each C# `[Theory]`/`[InlineData]`
row is carried across as an rstest `#[case(...)]`, so the **684 ≈ 685** counts line up essentially exactly.

## Deferrals / intentional differences

- **FileSystemMetadata / path seam** (`io.rs`, `path.rs`): C# relies on `System.IO`/`IFileSystem`; ported as a
  small local POCO + path helpers rather than pulling a filesystem abstraction. No behavioral difference for parsing.
- **Localization seam** (`external_files/localization.rs`): C#'s `ILocalizationManager` is represented by a
  `LocalizationManager` trait so `ExternalPathParser` stays testable without the full Jellyfin localization stack.
- **Regex engine**: uses `regex` + `fancy-regex` (for backreference/look-around patterns the default `regex`
  crate can't express); the pattern strings themselves are copied verbatim from the C# `NamingOptions`.
- No functional test cases were dropped. The ~1-count delta vs C# is bookkeeping (a couple of C# Facts collapse
  into shared rstest fixtures / MemberData rows), not skipped coverage.
