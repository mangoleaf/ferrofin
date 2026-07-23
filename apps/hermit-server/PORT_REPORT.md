# hermit-server — Port Report (INTEGRATE, Wave 8)

Port of `Jellyfin.Server` — the **composition root**. `Program.Main` + `Startup` +
`CoreAppHost.RegisterServices` become: bootstrap config resolution → open + migrate
the SQLite DB → discover ffmpeg → wire every concrete `hermit-core` manager into
`hermit-api`'s `AppState` → seed a default administrator on a fresh install → mount
the router → `axum::serve` with graceful shutdown.

This is a **composition root** (mostly wiring), so — mirroring the `hermit-traits`
definitions-crate precedent — its gate is **"boots + the First-Light integration
test passes + fmt/clippy clean"**, and it is **exempt from the 80% line-coverage
gate**: it is deliberately **NOT** on `.port/gated-crates.txt`. The exemption is
recorded in `brain/DEFERRED.md`. The `StreamStatePlanner` / config logic (the
non-wiring judgment code) **is** unit-tested regardless (see below).

## Gate results (INTEGRATE)

All commands run at the workspace root `/home/mango/dev/hermit`.

| Gate | Command | Result |
|------|---------|--------|
| Format apply | `cargo fmt --all` | PASS (exit 0) |
| Format check | `cargo fmt --all --check` | PASS (exit 0, no diff) |
| Build | `cargo build --workspace` | PASS (exit 0) |
| Clippy | `cargo clippy --all-targets --all-features -- -D warnings` | PASS (exit 0, no warnings) |
| Tests | `cargo test --workspace` | PASS (exit 0) — every crate green; `hermit-server` lib **54 passed**, `first_light` **1 passed** |
| Contract-superset | `cargo test -p hermit-api --test contract_superset` | PASS (exit 0) — **4 passed** (spec↔table both directions, probes never 404, no dup real routes) |
| First-Light | `first_light::first_light_client_flow` | PASS — full anonymous→auth→items→playback-info→ranged-stream flow |
| ffmpeg integration | `HERMIT_FFMPEG_TESTS=1 cargo test -p hermit-mediaencoding -p hermit-hls --test segment_transcode_ffmpeg --test hls_stream_manager_ffmpeg` | PASS (exit 0) — **8 passed** (real ffmpeg 8.1.1: TS + fMP4 transcode → segments + versioned playlist; end-to-end master variant + real segment) |

No gate failed.

## Per-crate coverage gate (the 15 gated crates)

Run individually — one `-p` per crate, **never** a merged multi-`-p` run — as
`HERMIT_FFMPEG_TESTS=1 cargo llvm-cov nextest -p <crate> --fail-under-lines 80 --summary-only`,
looping over `.port/gated-crates.txt`. **All 15 pass (exit 0), all ≥80% lines:**

| Crate | Lines | Gate |
|-------|-------|------|
| hermit-util | 94.49% | PASS |
| hermit-keyframes | 99.40% | PASS |
| hermit-model | 84.89% | PASS |
| hermit-common | 87.04% | PASS |
| hermit-naming | 96.49% | PASS |
| hermit-networking | 87.75% | PASS |
| hermit-health | 97.08% | PASS |
| hermit-db | 100.00% | PASS |
| hermit-mediaencoding | 83.54% | PASS |
| hermit-drawing | 91.32% | PASS |
| hermit-providers | 90.89% | PASS |
| hermit-livetv | 89.74% | PASS |
| hermit-hls | 91.62% | PASS |
| hermit-core | 83.18% | PASS |
| hermit-api | 83.68% | PASS |

Lowest: `hermit-core` 83.18%, `hermit-mediaencoding` 83.54%. `hermit-server` is
**not** in this list (composition-root exemption).

## hermit-server coverage (information only — exempt)

`HERMIT_FFMPEG_TESTS=1 cargo llvm-cov nextest -p hermit-server --summary-only` →
55 tests, **88.78% lines** (90.64% region, 88.78% line). Even though exempt, the
assembled crate clears 80% on its own. Per-module:

| Module | Lines | #[test] | Role |
|--------|-------|---------|------|
| `state.rs` | 100.00% | 2 | manager-wiring composition root + `HermitLifecycleController` |
| `seed.rs` | 97.87% | 4 | fresh-install admin seeding |
| `config.rs` | 97.34% | 13 | bootstrap config resolution (CLI > env > file > default) — **logic, unit-tested** |
| `bootstrap.rs` | 92.20% | 19 | logging / DB open / ffmpeg discovery |
| `planner.rs` | 86.15% | 8 | `HermitStreamStatePlanner` (the `StreamStatePlanner` seam) — **logic, unit-tested** |
| `media_encoding.rs` | 82.74% | 8 | transcode-pair + attachment-io assembly |

The exemption covers only pure wiring: the two modules with real judgment logic —
the **`StreamStatePlanner`** (media resolution → `EncodingJobInfo` → the exact HLS
ffmpeg command line; Jellyfin's `StreamingHelpers.GetStreamingState` +
`EncodingHelper.GetCommandLineArguments`) and **bootstrap config resolution** — are
directly unit-tested (86.15% / 97.34%), as the Wave-8 mandate requires.

## Does the assembled server boot and serve First-Light for real?

**Yes.** The `first_light` integration test boots the *actual* composition root
(`state::build_app_state` → the same `AppState` the binary serves) over a fresh temp
SQLite DB (real migrations), seeds the real admin, saves two scanned movie items via
the production persistence path, then drives the exact Jellyfin first-contact flow
through the real `hermit_api::create_router` (via `tower::oneshot`, no stubs):

1. `GET /System/Info/Public` → 200 (anonymous, server identity present)
2. `POST /Users/AuthenticateByName` → 200 `AuthenticationResult` (user + session)
3. `GET /Users/Me` (session token) → 200, returns the authenticated admin
4. `GET /Items` → 200, both seeded items present
5. `GET`/`POST /Items/{id}/PlaybackInfo` → 200, `MediaSources` carry the on-disk path
6. `GET /Videos/{id}/stream` with `Range: bytes=0-3` → **206 Partial Content** +
   `Content-Range: bytes 0-3/…`, serving exactly the 4 requested bytes (real
   direct-play file serving).

## Is the transcode pair live (not the Disabled stub)?

**Yes — the real pair is wired.** `state::build_app_state` builds `AppState::new`
(which installs the disabled stubs) and then **replaces** the media-encoding seams
via `AppState::with_media_encoding(hls, attachments)` with the concrete pair from
`media_encoding::build_media_encoding`:

- **HLS:** `HlsStreamManagerImpl` (not `DisabledHlsStreamManager`) wiring
  `HermitStreamStatePlanner` → `TokioSegmentTranscoder` → `TranscodeManagerImpl` →
  `DynamicHlsPlaylistGenerator`. This is the real ffmpeg-backed transcode runtime.
- **Attachments:** `AttachmentExtractorImpl` over a real ffmpeg/filesystem
  `AttachmentIo` + a `MediaSourceManager`-backed resolver (not
  `DisabledAttachmentExtractor`).

`hermit-server`'s own tests assert this (`build_media_encoding_produces_real_pair`
returns real trait objects, not stubs), and the ffmpeg-gated integration tests
(above) exercise the same `TokioSegmentTranscoder` / `HlsStreamManagerImpl` chain
against real ffmpeg — TS and fMP4 transcodes produce actual segments + playlists.

### Honest scope caveats (transcode)

The pair is **live**, but its First-Light scope is deliberately narrow (deferred
work tracked in `brain/DEFERRED.md`):

- The planner honours request-declared codecs and **stream-copies when it can**
  (`can_stream_copy_video`/`_audio` → `-c:v copy`/`-c:a copy` remux); it does **not**
  do full device-profile negotiation, the hardware-accel matrix
  (`NoOptionalEncoders` = software encoders only), HDR/tonemap/3D filters, or
  subtitle-provider fan-out.
- `DynamicHlsPlaylistGenerator` uses `EncodingOptions::default()` per request — the
  persisted named-config accessor is not yet threaded through
  `ServerConfigurationManager`. Master-playlist adaptive-bitrate is single-variant.
- Job progress → session-layer reporting is a `NoopSessionReporter` (killed-job
  partial-file cleanup is handled by the manager's `FsFileCleaner`).
- `ffmpeg` discovery is non-fatal: if no ffmpeg is found the encoder is wired with
  bare `ffmpeg`/`ffprobe` names, so the API still **boots** and playback 500s (not
  boot-fails) until a working ffmpeg is configured. On this host ffmpeg 8.1.1 is
  present and the integration tests pass.

## Honesty statement

Every INTEGRATE gate passes (exit 0): fmt apply + check, workspace build, clippy
(`-D warnings`, no warnings), workspace tests, the 4 contract-superset tests, the
First-Light integration test, and the ffmpeg-gated integration tests (real ffmpeg
8.1.1). All 15 gated crates pass their individual `--fail-under-lines 80` gate
(lowest 83.18%). `hermit-server` is exempt from the coverage gate as a composition
root (documented in `brain/DEFERRED.md`, kept off `.port/gated-crates.txt`), but its
`StreamStatePlanner` and config logic are unit-tested and the assembled crate clears
80% (88.78%) regardless. The assembled server boots and serves the real First-Light
client flow, and the transcode/HLS pair injected at the composition root is the live
`HlsStreamManagerImpl` chain, not the `Disabled` stub.
