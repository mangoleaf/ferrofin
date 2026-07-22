# hermit-drawing — Port Report

Port of `Jellyfin.Drawing` + `Emby.Photos` onto the pure-Rust `image` crate.
Scope: the `ImageProcessor` service, two `ImageEncoder` implementations
(real + null), and the photo embedded-information provider. No native Skia.

## Gate results (2026-07-22)

| Check | Command | Result |
|-------|---------|--------|
| Format | `cargo fmt --all` | applied, no diff |
| Format check | `cargo fmt --all --check` | **PASS** (exit 0) |
| Build | `cargo build --workspace` | **PASS** — `hermit-drawing` compiles clean |
| Clippy | `cargo clippy --all-targets --all-features -- -D warnings` | **PASS for hermit-drawing** (scoped `-p hermit-drawing` exits 0); the workspace-wide run fails only in **`hermit-providers`** (22 lints, unrelated crate/Wave) |
| Tests | `cargo test --workspace` | `hermit-drawing`: **53 passed, 0 failed**. Workspace-wide build breaks in **`hermit-mediaencoding`** (missing `encoder_validator/tests/data.rs`, unrelated crate/Wave) |
| Coverage | `cargo llvm-cov nextest -p hermit-drawing --fail-under-lines 80 --summary-only` | **PASS** — 91.32% lines (threshold 80, exit 0) |

**Honest note on the workspace gate:** the two workspace-wide commands
(`clippy --all-targets` and `test --workspace`) do **not** exit 0, but every
failure is outside this crate — `hermit-providers` (clippy lints) and
`hermit-mediaencoding` (a missing test data file). Both are other Waves' crates.
Scoped to `hermit-drawing` (`-p hermit-drawing`) all six checks pass. The gate
for this INTEGRATE stage — hermit-drawing itself — is green.

## What was implemented

Four modules, all with module-level port notes documenting each C# → Rust
decision:

- **`processor.rs`** — `ImageProcessor`, port of `Jellyfin.Drawing.ImageProcessor`.
  Orchestration only: owns an `Arc<dyn ImageEncoder>` + the image-cache dir, and
  drives dimension probing, blurhash component math, cache-tag / cache-key
  derivation, and the on-the-fly `process_image` resize pipeline (produce →
  cache → hit). Pure string/hash/gating on top of the encoder. Filesystem reads
  (`File.Exists`, `GetLastWriteTimeUtc`) sit behind a `FileMeta` seam (`StdFileMeta`
  in prod, fake in tests). `.NET`-ticks ↔ Unix-epoch conversion ported with the
  `621_355_968_000_000_000` constant. Generic over `F: FileMeta`.
- **`image_encoder.rs`** — `ImageCrateEncoder`, real codec, port of
  `SkiaEncoder` re-implemented on the `image` crate. Resize (which-axis-wins),
  format-convert, collage ratio dispatch, trickplay grid packing, and the
  "default options → return original path" short-circuit. Resize/crop math is
  delegated to shared `hermit-model::drawing::drawing_utils` so it matches the C#
  `DrawingUtils` exactly.
- **`null_encoder.rs`** — `NullImageEncoder`, port of `NullImageEncoder`. The
  fallback that advertises formats/capabilities but returns
  `ServiceError::Backend` (port of `NotImplementedException`) from every
  pixel-touching method.
- **`photo_provider.rs`** — `PhotoProvider`, port of `Emby.Photos.PhotoProvider`.
  A plain struct exposing `name` / `has_changed` (file-protocol-gated last-write
  comparison behind a `DirectoryService` seam) / `fetch` (set Primary image path,
  backfill non-positive width/height from the `ImageProcessor`).

## Traits satisfied

- `hermit_traits::drawing::ImageEncoder` — implemented by **`ImageCrateEncoder`**
  and **`NullImageEncoder`** (all 10 methods: `supported_input_formats`,
  `supported_output_formats`, `name`, `supports_image_collage_creation`,
  `supports_image_encoding`, `get_image_size`, `get_image_blur_hash`,
  `encode_image`, `create_image_collage`, `create_splashscreen`,
  `create_trickplay_tile`).
- `hermit_traits::drawing::ImageProcessor` — implemented by **`ImageProcessor`**
  (all methods: format/collage/output accessors, `get_image_dimensions`,
  `get_item_image_dimensions`, blurhash + sized, cache-tag by id and by path,
  `process_image`, `create_image_collage`).
- Local `FileMeta` seam trait (prod `StdFileMeta`) and `DirectoryService` seam
  trait for hermetic tests.

## Tests

**53 tests, all passing** (`cargo test -p hermit-drawing` and
`cargo llvm-cov nextest`):

- `processor.rs` — 14: cache-tag stability/sensitivity, ticks↔epoch constant,
  cache-key version + per-option variance, dimension info-vs-probe selection,
  passthrough/short-circuit gates, resize-then-cache-hit, format advertisement.
- `image_encoder.rs` — 17: resize dimension parity (1920×1080 → 960×540),
  PNG→JPEG decode-at-target, probe fixture/undecodable-default dims, square &
  thumb collage output, trickplay grid pack + all invalid-input arms,
  default-options passthrough.
- `null_encoder.rs` — 11: every real method errors; capability/format/name
  accessors return the fixed values.
- `photo_provider.rs` — 11: `has_changed` truth table (missing file, non-file
  protocol, disk-newer, not-newer), name constant, dimension backfill
  (zero-width probes, positive dims left alone, probe-error swallow/propagate by
  format).

## Coverage

91.32% lines overall (`--fail-under-lines 80` satisfied):

| File | Line cover | Fn cover |
|------|-----------|----------|
| `null_encoder.rs` | 100.00% | 100.00% |
| `image_encoder.rs` | 96.49% | 90.14% |
| `photo_provider.rs` | 90.36% | 74.47% |
| `processor.rs` | 86.21% | 73.33% |
| **TOTAL** | **91.32%** | 82.08% |

Uncovered lines are the intentionally-excluded seams and defensive arms (see
Deferrals): `StdFileMeta`'s real `std::fs` stat, `output_format` Jpg
fall-through, the `write!` cache-key error map, the encode-error-to-original
fallback, the `_assert_object_safe_*` object-safety marker fns, and defensive
error arms in `photo_provider`.

## Parity vs. the C# xUnit oracle

- **Dimensional / structural parity, not byte-exact.** The `image` crate and
  Skia use different resamplers and JPEG/PNG/WebP writers, so there is no byte
  oracle. What is ported exactly is the *shape* of each operation: which axis
  wins on resize (verified 1920×1080 → 960×540), collage ratio dispatch,
  trickplay grid packing, and the default-options short-circuit. Resize/crop math
  is shared with `hermit-model` `drawing_utils`, matching `DrawingUtils` /
  `ImageHelper.GetNewImageSize`.
- **Cache tags/keys are exact.** `GetMD5().ToString("N")` → `hermit_common`
  `get_md5` + `Uuid::simple` (32-char lowercase hex, identical to .NET `"N"`);
  `.NET`-ticks epoch offset uses the canonical `621_355_968_000_000_000`
  constant; cache `Version = '3'` is preserved (as a configurable default).
- **Error mapping.** C# `throw` sites map onto the flat `ServiceError` taxonomy:
  `FileNotFoundException` → `NotFound`, `InvalidDataException`/`ArgumentException`
  → `InvalidInput`, `NotImplementedException`/I-O/encode → `Backend`. `default;`
  fallbacks → `ImageDimensions::default()` (0×0).

## Deferrals

- **EXIF metadata mapping (photo_provider).** The conditional EXIF branch
  (aperture/shutter/make/model/rating/orientation/lat-long-alt/ISO/…) is
  deferred: it needs both a `Photo` domain entity and the
  `ICustomMetadataProvider` provider trait, neither of which exists yet
  (provider layer is deferred to `hermit-core` match logic). The extension gate
  `is_exif_candidate` (`_includeExtensions`) is ported and unit-tested so the
  branch drops in later without reshaping the file. No `kamadak-exif` dep added.
- **`Photo` auto-orientation (processor).** `auto_orient` is always `false`;
  the gate is preserved structurally, to re-enable once the `Photo` entity lands.
- **Blurhash + splashscreen (image_encoder).** No oracle in this unit; return
  `ServiceError::Backend`.
- **`ParallelImageEncodingLimit` semaphore.** Dropped — concurrency limiting is
  the host's concern, no oracle.
- **Skia-only input formats** (`dng`/`astc`/`ktx`/raw/`svg`) and the
  `EncodeImage` overlay work (background/blur/foreground/played indicators) are
  not ported; this encoder does resize + format-convert only.
- **`FileMeta` real `std::fs` stat** is behind the seam and intentionally
  excluded from parity/coverage.

See `brain/PLAN_HERMIT_PORT.md` for the Wave 5 PortJob context.
