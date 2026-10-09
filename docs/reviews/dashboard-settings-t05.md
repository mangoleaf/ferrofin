# T05 — allowed HEVC and AV1 encoder selection

The planner now selects the first codec after the existing source-compatible allow-flag reorder. A second private filter previously removed software HEVC/AV1 targets, so saving `AllowHevcEncoding` or `AllowAv1Encoding` did not enable those software jobs. The corrected selection reaches the existing software or hardware encoder dispatcher.

Primary source is Jellyfin `4910aafa1a`, `AttachMediaSourceInfo`, `ShiftVideoCodecsIfNeeded` and `GetVideoEncoder`; web pin is `1e507c588f353482a00333f84e36ddb7c8fc8221`. These flags reorder a disallowed leading target when an allowed alternative exists. They preserve a sole target and an all-disallowed list, and do not prohibit direct copy. The first generic codec, including VP9, also remains first. This intentionally removes Ferrofin's private VP9 skip along with its software HEVC/AV1 filter.

Persisted planner regressions cover target order, HEVC/H265 aliases, AV1, single/all-disallowed lists, copy, and HDR master-playlist entrances. Actual-binary observations exercise six software codec selections and a generated 10-bit PQ HEVC source with copy/master behavior under saved flag changes. The recording executable passes unchanged arguments to actual FFmpeg, limits only its own CPU affinity, and retains invocation and response bytes. Supporting Jellyfin 12.1.0 remains distinct from the source pin. No hardware/GPU execution claim is made.

Production changes are confined to server planning, which has its compile/integration coverage exemption. Planner, real HTTP/native, lint and documentation gates remain required; earlier thread, preset and path settings are preserved.

The first native attempt exposed an SDR test fixture: this FFmpeg build erased transfer/primaries when generic output flags were combined with x265 VUI parameters. The corrected fixture uses explicit x265 PQ/BT.2020 metadata and verifies both raw ffprobe output and each server's HDR classification before checking playlist entrances. Failed fixture/verifier attempts remain retained.

Validation passed:

- fmt: passed (1.98 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (23.53 s, including any compilation).
- build: passed (5.64 s, including any compilation).
- native-before: passed (6.49 s, including any compilation).
- native-reference: passed (10.68 s, including any compilation).
- native-after: passed (7.29 s, including any compilation).
- clippy: passed (7.64 s, including any compilation).
- doctests: 3 passed, 0 ignored (8.90 s, including any compilation).
- server: 90 tests run: 90 passed, 82 skipped (7.10 s, including any compilation).

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| startup / startup.startup_ms | 4833.669 | 4808.230 | 5372.088 |
| fixture_media_main / generation_ms | 36.827 | 37.711 | 38.394 |
| codec_0 / save.milliseconds | 21.205 | 11.036 | 16.402 |
| codec_0 / encoding.milliseconds | 1.398 | 0.869 | 8.485 |
| codec_0 / operation.playlist.milliseconds | 3.507 | 2.571 | 73.033 |
| codec_0 / operation.segment.milliseconds | 45.666 | 47.645 | 138.612 |
| codec_0 / stop.milliseconds | 1.728 | 1.946 | 9.000 |
| codec_1 / save.milliseconds | 8.282 | 7.535 | 6.249 |
| codec_1 / encoding.milliseconds | 1.459 | 0.872 | 5.792 |
| codec_1 / operation.playlist.milliseconds | 3.351 | 2.983 | 12.301 |
| codec_1 / operation.segment.milliseconds | 44.073 | 103.374 | 217.427 |
| codec_1 / stop.milliseconds | 1.672 | 6.776 | 7.053 |
| codec_2 / save.milliseconds | 7.178 | 15.928 | 6.027 |
| codec_2 / encoding.milliseconds | 0.944 | 0.878 | 5.746 |
| codec_2 / operation.playlist.milliseconds | 2.884 | 2.789 | 9.361 |
| codec_2 / operation.segment.milliseconds | 40.557 | 45.165 | 113.033 |
| codec_2 / stop.milliseconds | 1.370 | 1.714 | 7.156 |
| codec_3 / save.milliseconds | 6.174 | 7.506 | 5.956 |
| codec_3 / encoding.milliseconds | 0.772 | 0.900 | 5.550 |
| codec_3 / operation.playlist.milliseconds | 2.689 | 2.832 | 9.328 |
| codec_3 / operation.segment.milliseconds | 40.391 | 300.990 | 314.198 |
| codec_3 / stop.milliseconds | 1.261 | 7.629 | 8.208 |
| codec_4 / save.milliseconds | 7.413 | 15.742 | 6.177 |
| codec_4 / encoding.milliseconds | 0.737 | 0.759 | 6.335 |
| codec_4 / operation.playlist.milliseconds | 2.435 | 3.120 | 9.814 |
| codec_4 / operation.segment.milliseconds | 41.967 | 106.075 | 212.914 |
| codec_4 / stop.milliseconds | 2.802 | 6.840 | 6.710 |
| codec_5 / save.milliseconds | 50.255 | 82.680 | 5.867 |
| codec_5 / encoding.milliseconds | 1.618 | 1.403 | 5.643 |
| codec_5 / operation.playlist.milliseconds | 3.510 | 3.601 | 9.533 |
| codec_5 / operation.segment.milliseconds | 42.836 | 292.270 | 313.785 |
| codec_5 / stop.milliseconds | 1.726 | 6.613 | 12.232 |
| fixture_media_hdr-hevc / generation_ms | 80.818 | 79.152 | 73.457 |
| hdr_hevc_false / save.milliseconds | 7.589 | 7.795 | 4.306 |
| hdr_hevc_false / encoding.milliseconds | 0.804 | 1.282 | 3.874 |
| hdr_hevc_false / master.response.milliseconds | 3.265 | 4.459 | 19.017 |
| hdr_hevc_false / operation.playlist.milliseconds | 2.603 | 2.772 | 29.028 |
| hdr_hevc_false / operation.segment.milliseconds | 36.727 | 36.683 | 109.382 |
| hdr_hevc_false / stop.milliseconds | 1.089 | 1.051 | 4.383 |
| hdr_hevc_true / save.milliseconds | 6.934 | 6.190 | 4.256 |
| hdr_hevc_true / encoding.milliseconds | 0.852 | 0.870 | 3.566 |
| hdr_hevc_true / master.response.milliseconds | 3.115 | 3.414 | 6.027 |
| hdr_hevc_true / operation.playlist.milliseconds | 2.261 | 2.641 | 9.022 |
| hdr_hevc_true / operation.segment.milliseconds | 34.236 | 34.893 | 107.055 |
| hdr_hevc_true / stop.milliseconds | 1.028 | 1.176 | 4.333 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t05/checks.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.

