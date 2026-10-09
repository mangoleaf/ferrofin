# T08: maximum HLS muxing queue

`MaxMuxingQueueSize` now reaches each newly started HLS ffmpeg command as the
output option `-max_muxing_queue_size`. The saved signed integer is floored at
128, matching `DynamicHlsController.GetCommandLineArguments` in Jellyfin
`4910aafa1a` lines 1627–1650. The model's default remains 2048. No upper clamp or
rejection replaces the upstream value; negative, zero and values through 128
all select 128, while 129 and larger pass through exactly.

The option applies to video and audio-only HLS, copied and re-encoded streams,
VOD and EVENT playlists, and MPEG-TS and fMP4 segments. It appears once after
input arguments and before `-f hls`. Running processes retain the command with
which they started. A subsequent configuration save affects newly started
processes, as it does upstream.

The prepared unit test builds 64 actual plans crossing eight signed boundary
values, two playlist kinds, two containers and audio/video. It checks the
exact value, one flag occurrence, and output-option placement. Existing planner
tests cover copied and encoded stream paths. The real HTTP verification must
save encoding settings, start distinct jobs through the HLS routes, consume
nonempty segments, and inspect the real process/log argument vector at default
2048, configured 1024/4096, the signed/extreme boundary values, and floor cases 0 and 128. Use a fresh playback-session
and query identity per job, or explicitly stop the previous job, so an existing
transcode does not mask a newly saved command setting.

The prepared actual-runtime fixture uses the root's recording encoder to capture
real ffmpeg argv while executing ffmpeg, plus actual save/readback/stop HTTP
responses and consumed segment bytes. Thirteen distinct jobs cover the default,
eight signed boundaries, TS/fMP4, VOD/EVENT, and copied/re-encoded video. Its
separate verifier requires the exact argument value and placement for reference
and after runs, and the exact missing argument for the before run. This remains
prepared evidence until root runs it.

The primary source has no reference to this setting in progressive output
commands. T06's prepared implementation now provides a separate progressive
plan and single-file muxer, returning before the HLS muxer; T08 therefore also
prepares four actual progressive audio/video plans with changed queue values
and asserts that neither the queue option nor HLS segment options appear.
This finding depends on the T06 progressive implementation passing its own
validation before these exclusions can be claimed as implemented behavior.

The initial native verifier reused a TS-only packet check for fMP4 responses.
It now checks TS packet framing for TS and complete ISO BMFF box boundaries with
real `moof`/`mdat` boxes for fMP4; response bytes, argv and queue assertions are
retained. The failed baseline attempt is preserved.

Validation passed:

- fmt: passed (2.31 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (26.45 s, including any compilation).
- build: passed (2.06 s, including any compilation).
- native-before: passed (8.50 s, including any compilation).
- native-reference: passed (10.08 s, including any compilation).
- native-after: passed (8.28 s, including any compilation).
- clippy: passed (9.07 s, including any compilation).
- doctests: 3 passed, 0 ignored (10.59 s, including any compilation).
- server: 100 tests run: 100 passed, 82 skipped (8.77 s, including any compilation).

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| startup / startup.startup_ms | 6507.930 | 6477.750 | 5906.765 |
| fixture_media_main / generation_ms | 46.562 | 45.767 | 50.119 |
| default / save.milliseconds | 10.362 | 6.495 | 12.871 |
| default / encoding.milliseconds | 1.878 | 0.743 | 5.386 |
| default / operation.playlist.milliseconds | 4.242 | 3.595 | 39.512 |
| default / operation.segment.milliseconds | 51.781 | 56.147 | 134.991 |
| default / stop.milliseconds | 8.398 | 10.419 | 7.292 |
| boundary_0 / save.milliseconds | 7.484 | 10.841 | 4.882 |
| boundary_0 / encoding.milliseconds | 2.151 | 2.763 | 4.382 |
| boundary_0 / operation.playlist.milliseconds | 7.178 | 12.482 | 10.093 |
| boundary_0 / operation.segment.milliseconds | 52.813 | 55.571 | 117.718 |
| boundary_0 / stop.milliseconds | 9.007 | 8.972 | 5.923 |
| boundary_1 / save.milliseconds | 7.748 | 7.043 | 5.026 |
| boundary_1 / encoding.milliseconds | 1.129 | 0.795 | 4.986 |
| boundary_1 / operation.playlist.milliseconds | 56.634 | 46.926 | 121.534 |
| boundary_1 / operation.segment.milliseconds | 1.526 | 1.461 | 5.126 |
| boundary_1 / stop.milliseconds | 7.545 | 8.402 | 5.070 |
| boundary_2 / save.milliseconds | 7.141 | 7.033 | 4.758 |
| boundary_2 / encoding.milliseconds | 1.207 | 0.991 | 5.349 |
| boundary_2 / operation.playlist.milliseconds | 3.566 | 2.976 | 9.352 |
| boundary_2 / operation.segment.milliseconds | 51.920 | 49.117 | 113.115 |
| boundary_2 / stop.milliseconds | 12.230 | 8.803 | 5.761 |
| boundary_3 / save.milliseconds | 7.597 | 6.613 | 5.850 |
| boundary_3 / encoding.milliseconds | 1.537 | 0.680 | 5.029 |
| boundary_3 / operation.playlist.milliseconds | 47.199 | 44.814 | 110.679 |
| boundary_3 / operation.segment.milliseconds | 1.833 | 0.965 | 4.349 |
| boundary_3 / stop.milliseconds | 7.316 | 8.575 | 5.167 |
| boundary_4 / save.milliseconds | 9.286 | 6.083 | 4.885 |
| boundary_4 / encoding.milliseconds | 2.436 | 0.856 | 4.713 |
| boundary_4 / operation.playlist.milliseconds | 6.499 | 2.684 | 17.285 |
| boundary_4 / operation.segment.milliseconds | 50.952 | 44.947 | 112.798 |
| boundary_4 / stop.milliseconds | 8.754 | 8.869 | 9.146 |
| boundary_5 / save.milliseconds | 7.910 | 10.265 | 9.808 |
| boundary_5 / encoding.milliseconds | 1.264 | 0.824 | 6.885 |
| boundary_5 / operation.playlist.milliseconds | 53.728 | 52.199 | 114.926 |
| boundary_5 / operation.segment.milliseconds | 1.568 | 1.683 | 4.008 |
| boundary_5 / stop.milliseconds | 7.865 | 7.610 | 5.594 |
| boundary_6 / save.milliseconds | 10.751 | 6.140 | 4.863 |
| boundary_6 / encoding.milliseconds | 1.025 | 0.809 | 4.461 |
| boundary_6 / operation.playlist.milliseconds | 3.275 | 2.945 | 7.194 |
| boundary_6 / operation.segment.milliseconds | 49.914 | 58.081 | 110.983 |
| boundary_6 / stop.milliseconds | 9.384 | 8.849 | 5.894 |
| boundary_7 / save.milliseconds | 9.140 | 6.578 | 4.872 |
| boundary_7 / encoding.milliseconds | 2.513 | 1.070 | 5.506 |
| boundary_7 / operation.playlist.milliseconds | 49.178 | 50.073 | 110.751 |
| boundary_7 / operation.segment.milliseconds | 1.818 | 2.182 | 2.760 |
| boundary_7 / stop.milliseconds | 8.554 | 12.268 | 4.381 |
| fragmented_floor / save.milliseconds | 8.470 | 6.826 | 4.815 |
| fragmented_floor / encoding.milliseconds | 2.051 | 1.361 | 5.650 |
| fragmented_floor / operation.playlist.milliseconds | 4.935 | 3.643 | 7.731 |
| fragmented_floor / operation.segment.milliseconds | 49.618 | 47.156 | 110.650 |
| fragmented_floor / stop.milliseconds | 8.566 | 8.501 | 5.160 |
| fragmented_configured / save.milliseconds | 7.846 | 6.501 | 5.145 |
| fragmented_configured / encoding.milliseconds | 1.405 | 1.087 | 4.620 |
| fragmented_configured / operation.playlist.milliseconds | 59.564 | 52.137 | 122.152 |
| fragmented_configured / operation.segment.milliseconds | 1.492 | 1.640 | 3.555 |
| fragmented_configured / stop.milliseconds | 7.959 | 7.571 | 4.604 |
| copy_transport / save.milliseconds | 6.636 | 6.759 | 4.735 |
| copy_transport / encoding.milliseconds | 1.101 | 0.701 | 4.217 |
| copy_transport / operation.playlist.milliseconds | 3.718 | 2.692 | 27.909 |
| copy_transport / operation.segment.milliseconds | 43.921 | 49.897 | 113.732 |
| copy_transport / stop.milliseconds | 1.346 | 1.326 | 5.413 |
| copy_fragmented / save.milliseconds | 7.330 | 6.224 | 4.969 |
| copy_fragmented / encoding.milliseconds | 0.851 | 0.729 | 4.451 |
| copy_fragmented / operation.playlist.milliseconds | 43.051 | 39.257 | 109.601 |
| copy_fragmented / operation.segment.milliseconds | 2.835 | 1.356 | 4.155 |
| copy_fragmented / stop.milliseconds | 2.264 | 0.955 | 5.124 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t08/checks.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.

Final complete workspace matrix: **8,407 passed, 5 skipped, 1 slow** across all 21 packages and 201 default test targets. Mediaencoding, HLS and server were rerun with real FFmpeg enabled. Eighteen independent packages retain their previous passing results, with all source inputs and every workspace dependency kind checked to exclude either changed package from their dependency closure. WASM guest skips remain qualified. Full commands, target enumeration and reuse proof: `/tmp/ferrofin-next10-root/evidence/final90-matrix/run-001`. Fresh formatting, strict workspace Clippy, three doctests and production build match all 895 current Rust/Cargo input hashes: `/tmp/ferrofin-next10-root/final90-quality-source-proof.json`.
