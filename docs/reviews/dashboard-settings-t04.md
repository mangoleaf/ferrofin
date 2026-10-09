# T04 — configured encoder preset and CRF

Saved `EncoderPreset.auto` now reaches the encoder's actual auto arm. Previously the private helper substituted its caller's default, giving EVENT x264/x265 `superfast` and VOD SVT-AV1 preset 11. Pinned behavior uses software `veryfast`, NVENC `p1` and SVT-AV1 preset 10 for configured auto. Explicit presets and codec-specific CRF rules remain separate: values from 0 through 51 are accepted, otherwise x264 falls back to 23 and x265 to 28.

Primary source is Jellyfin `4910aafa1a`, `EncodingOptions` and `EncodingHelper.GetEncoderParam` / `GetVideoQualityParam`; web pin is `1e507c588f353482a00333f84e36ddb7c8fc8221`. `EncodingOptions.EncoderPreset` is non-nullable and defaults to auto. The helper's nullable signature does not make saved null, empty, omitted and auto interchangeable. An omitted field uses auto; named configuration binding rejects explicit null or empty with a backend error, preserving the previous file.

Tests cover encoder auto arms against caller defaults, CRF boundaries, and actual persisted VOD/EVENT planner arguments. Actual-binary observations cover configured auto, omitted auto, medium, invalid CRF fallbacks, and exact null/empty HTTP responses with persisted file hashes. They collect real HLS bytes and launched FFmpeg arguments. Supporting Jellyfin 12.1.0 remains separate from the pinned source; hardware argument tests are not GPU execution certification.

Mediaencoding needs separate changed-crate coverage; server planner wiring has its compile/integration exemption and still runs regressions. The existing named enum model and prior findings are preserved.

Validation passed:

- fmt: passed (2.44 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (25.64 s, including any compilation).
- build: passed (6.43 s, including any compilation).
- native-before: passed (6.94 s, including any compilation).
- native-reference: passed (9.84 s, including any compilation).
- native-after: passed (6.69 s, including any compilation).
- clippy: passed (14.38 s, including any compilation).
- doctests: 3 passed, 0 ignored (13.35 s, including any compilation).
- ferrofin-mediaencoding: 35 tests run: 35 passed, 888 skipped (9.09 s, including any compilation).
- server: 89 tests run: 89 passed, 82 skipped (9.72 s, including any compilation).
- real-ffmpeg: 7 tests run: 7 passed, 0 skipped (8.08 s, including any compilation).

Separate changed-crate line coverage:

- ferrofin-mediaencoding: 17,173/18,868 lines (91.016536%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 9 functions have mismatched data.

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| startup / startup.startup_ms | 5356.061 | 5151.488 | 6353.291 |
| fixture_media_main / generation_ms | 44.031 | 47.089 | 41.301 |
| quality_0 / save.milliseconds | 7.424 | 6.607 | 14.302 |
| quality_0 / encoding.milliseconds | 2.116 | 0.944 | 6.666 |
| quality_0 / operation.playlist.milliseconds | 4.759 | 3.469 | 38.893 |
| quality_0 / operation.segment.milliseconds | 60.731 | 51.942 | 144.107 |
| quality_0 / stop.milliseconds | 3.239 | 2.052 | 9.544 |
| quality_1 / save.milliseconds | 8.030 | 6.602 | 6.420 |
| quality_1 / encoding.milliseconds | 1.291 | 1.018 | 6.209 |
| quality_1 / operation.playlist.milliseconds | 4.076 | 3.761 | 13.444 |
| quality_1 / operation.segment.milliseconds | 52.563 | 53.097 | 116.217 |
| quality_1 / stop.milliseconds | 2.064 | 2.286 | 7.594 |
| quality_2 / save.milliseconds | 7.040 | 6.553 | 7.536 |
| quality_2 / encoding.milliseconds | 1.544 | 1.185 | 8.868 |
| quality_2 / operation.playlist.milliseconds | 3.551 | 2.998 | 13.291 |
| quality_2 / operation.segment.milliseconds | 65.541 | 54.436 | 115.802 |
| quality_2 / stop.milliseconds | 2.437 | 2.261 | 7.978 |
| quality_3 / save.milliseconds | 8.389 | 6.811 | 6.773 |
| quality_3 / encoding.milliseconds | 2.040 | 1.012 | 6.582 |
| quality_3 / operation.playlist.milliseconds | 60.402 | 50.563 | 140.063 |
| quality_3 / operation.segment.milliseconds | 1.626 | 1.362 | 5.699 |
| quality_3 / stop.milliseconds | 2.175 | 1.635 | 6.476 |
| quality_4 / save.milliseconds | 7.101 | 6.306 | 6.347 |
| quality_4 / encoding.milliseconds | 1.162 | 1.386 | 6.470 |
| quality_4 / operation.playlist.milliseconds | 58.147 | 58.981 | 115.932 |
| quality_4 / operation.segment.milliseconds | 2.026 | 1.856 | 6.694 |
| quality_4 / stop.milliseconds | 2.042 | 2.207 | 6.503 |
| quality_5 / save.milliseconds | 6.277 | 7.156 | 6.315 |
| quality_5 / encoding.milliseconds | 1.188 | 0.873 | 6.083 |
| quality_5 / operation.playlist.milliseconds | 3.322 | 2.896 | 18.601 |
| quality_5 / operation.segment.milliseconds | 47.933 | 52.632 | 118.738 |
| quality_5 / stop.milliseconds | 1.995 | 2.019 | 6.888 |
| quality_6 / save.milliseconds | 6.718 | 6.527 | 6.040 |
| quality_6 / encoding.milliseconds | 1.621 | 1.095 | 5.550 |
| quality_6 / operation.playlist.milliseconds | 4.190 | 3.257 | 11.847 |
| quality_6 / operation.segment.milliseconds | 46.358 | 49.770 | 113.170 |
| quality_6 / stop.milliseconds | 1.874 | 1.832 | 6.903 |
| invalid_preset_null / save.milliseconds | 1.228 | 1.401 | 8.273 |
| invalid_preset_null / encoding.milliseconds | 0.954 | 1.061 | 5.653 |
| invalid_preset_empty / save.milliseconds | 0.957 | 1.174 | 9.536 |
| invalid_preset_empty / encoding.milliseconds | 0.719 | 1.095 | 5.573 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t04/checks.json` and `/tmp/ferrofin-next10-root/evidence/t04/coverage.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.

