# hermit-hls — Port Report

Port of `Jellyfin.MediaEncoding.Hls` (namespace `MediaBrowser.MediaEncoding.Hls`)
into the Hermit Rust workspace. This crate builds `.m3u8` HLS playlists on top of
`hermit-mediaencoding` and `hermit-keyframes`, mirroring the C# `Playlist.*`
layout.

## What was implemented

| Rust module | C# source | Notes |
|---|---|---|
| `create_main_playlist_request.rs` | `Playlist/CreateMainPlaylistRequest.cs` | Request DTO for the main playlist. |
| `dynamic_hls_playlist_generator.rs` | `Playlist/DynamicHlsPlaylistGenerator.cs` + `IDynamicHlsPlaylistGenerator.cs` | Generator + interface; parity-core timing helpers. |
| `error.rs` | exception surface | `HlsError::InvalidOperation` ports the sole `InvalidOperationException` thrown on the public path. |
| `lib.rs` | namespace root | Re-exports and crate docs. |

### Parity core (the test oracle)

The three static helpers carry the substance of the port and are the target of
the entire test oracle:

- `compute_equal_length_segments(desired_segment_length_ms, total_runtime_ticks)`
  — ports `ComputeEqualLengthSegments`; throws `InvalidOperation` on zero
  segment length or zero runtime.
- `compute_segments(keyframe_data, desired_segment_length_ms)` — ports
  `ComputeSegments`, including duration-overshoot clamping to the total duration.
- `is_extraction_allowed_for_file(file_path, allowed_extensions)` — ports
  `IsExtractionAllowedForFile`.

Public `DynamicHlsPlaylistGenerator::create_main_playlist` stitches these into
the `.m3u8` string (TS vs fMP4 container selection, version-7 + `EXT-X-MAP`
header for fMP4, equal-length fallback for remuxed/disallowed inputs).

Supporting parity details ported: `.NET` banker's rounding (`convert_to_i64`),
`TimeSpan.Ticks` constants (`TICKS_PER_SECOND`, `TICKS_PER_MILLISECOND`),
`.NET`-style double formatting helpers (ceiling / non-finite fallback), and
blank-container-defaults-to-`ts` behavior.

## Traits satisfied

- `KeyframeExtractor` (ports `Extractors.IKeyframeExtractor`) — mockable
  boundary for keyframe timing. `is_metadata_based` + `try_extract_keyframes`.
  The generator retains only metadata-based extractors at construction, mirroring
  `extractors.Where(e => e.IsMetadataBased)`.
- `EncodingOptionsProvider` (models `IServerConfigurationManager.GetEncodingOptions()`)
  — read per request so allowed-extension config is live, not captured at
  construction. Blanket impl for any `Fn() -> EncodingOptions`.

## Tests

**20 tests, all passing** (15 unit in `src/`, 5 integration in
`tests/create_main_playlist.rs`), 0 ignored.

Unit tests cover: equal-length valid/invalid, segment computation valid,
zero/minor duration-overshoot clamp, banker's rounding, extraction-allowed
valid/invalid, blank-container→ts default, fMP4 version-7 + map line, no-segments
targets desired length, equal-length TS emits no map, .NET-parity format helpers,
non-finite ceiling fallback.

Integration tests cover the public `create_main_playlist` surface: keyframe
segments on remux video, disallowed-extension fallback to equal-length, fMP4 map
header + version 7, equal-length TS three-segment playlist, zero-runtime-without-
keyframes → `InvalidOperation`.

### Real-ffmpeg end-to-end test (`tests/hls_stream_manager_ffmpeg.rs`)

`HlsStreamManagerImpl` — the composition of the playlist generator + the live
transcode runtime (`start_ffmpeg` / `wait_for_segment` / the real
`TokioSegmentTranscoder` spawn from `hermit-mediaencoding`) — is validated
end-to-end against a **live ffmpeg**. The test drives the *same seam the HTTP
layer serves*: it asks the manager for the master playlist, the variant
`main.m3u8`, then a dynamic segment, and asserts the returned `ServedFile` points
at a real, non-empty `.ts` segment (with the `0x47` mpegts sync byte) that a live
ffmpeg produced, then stops the encoding and asserts the partial files are
deleted. The un-ported request→plan glue (`StreamStatePlanner`) is supplied by a
test planner that generates a clip and emits real HLS args; everything below it is
production code.

It **self-skips** unless `HERMIT_FFMPEG_TESTS` is set AND `ffmpeg` is on `PATH`,
and never affects the coverage gate (integration `tests/` don't count toward
`cargo llvm-cov -p hermit-hls`). Run it with:

```
HERMIT_FFMPEG_TESTS=1 cargo test -p hermit-hls --test hls_stream_manager_ffmpeg
```

## Coverage

`cargo llvm-cov nextest -p hermit-hls --fail-under-lines 80 --summary-only`

| File | Line cover | Lines missed |
|---|---|---|
| `create_main_playlist_request.rs` | 100.00% | 0 |
| `dynamic_hls_playlist_generator.rs` | 98.88% | 4 |
| **TOTAL** | **98.94%** | **4** |

Functions 97.44% (1 missed), regions 99.31%. Comfortably above the 80% floor;
gate passed.

### Uncovered (honest accounting)

- `dynamic_hls_playlist_generator.rs:586-588` — the body of the `NoKeyframes`
  inline test-helper's `try_extract_keyframes`. Dead-by-design test scaffolding:
  tests constructing `NoKeyframes` exercise the equal-length path, which never
  consults the extractor, so this stub body is never invoked. It is test-only
  code, not production logic, and models the C# "every extractor returns false"
  case structurally.

## xUnit parity

The Rust test names mirror the C# xUnit fact/theory names one-to-one
(`compute_equal_length_segments_valid_success`,
`compute_equal_length_segments_invalid_throws_invalid_operation`,
`compute_segments_valid_success`, the duration-overshoot clamp theories,
`is_extraction_allowed_for_file_*`, etc.). The parity-core helpers reproduce the
C# arithmetic exactly (banker's rounding, tick constants, overshoot clamping,
.NET double formatting), so the oracle is a faithful translation rather than a
re-derivation. The C# `InvalidOperationException` maps to `HlsError::InvalidOperation`
carrying the same two operands in the message.

## Deferrals

- **Concrete keyframe extractors** (`FfProbeKeyframeExtractor`,
  `MatroskaKeyframeExtractor`) are out of scope for this unit. They are
  un-mockable process I/O (ffprobe / Matroska spawn); the `KeyframeExtractor`
  trait keeps that boundary out of the parity/coverage numbers. Real impls shell
  out; unit/integration tests supply fakes.

## Gate results

| Command | Result |
|---|---|
| `cargo fmt --all` | clean |
| `cargo fmt --all --check` | clean |
| `cargo build --workspace` | ok |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo test --workspace` | ok |
| `cargo llvm-cov nextest -p hermit-hls --fail-under-lines 80 --summary-only` | 98.94% line, gate passed |
