# T06 — live transcode throttling

`EnableThrottling` and `ThrottleDelaySeconds` now control eligible live encoder jobs. A five-second controller reads the saved settings on each tick, clamps delay to 60 seconds, and resumes an already paused encoder when disabled. Enrollment follows startup and checks File protocol, known runtime of at least five minutes, video input, the separate encoding-state VideoFile default, and actual keyboard capability. Successful writes alone change pause state; failures are logged and retried. Teardown cancels scheduling and resumes before graceful quit under the same controller lock.

Primary behavior is pinned to Jellyfin `4910aafa1a`: TranscodingThrottler, TranscodeManager, EncodingHelper, JobLogger, DynamicHlsController and TranscodingJob. The supporting runtime is Jellyfin 12.1.0 and remains separate from that source pin. Real interactive help selects p/u; otherwise exact .NET version ordering permits legacy c/newline through version 6.1, excluding 6.1.0. The startup help probe has a bounded 15-second timeout, an explicit operational qualification relative to upstream's unbounded wait.

Actual CR/LF stderr statistics supply encoded position, seek offset, percentage, fps, size and bitrate. HLS tracks the monotonic maximum of client RuntimeTicks plus ActualSegmentLengthTicks when the response completes or is dropped, including aborted transfers. Progressive responses count bytes actually read and use encoded-byte/file-size fallback. Captured response observers retain the producing job across seek replacement. Session pings are not download progress. The API keeps its model/traits dependency boundary and SQL ceilings remain unchanged.

The necessary progressive repair routes ordinary Static=false requests through a genuine single growing output file; these requests previously served the existing source file or fell back to an HLS playlist. Static=true retains source serving. Video/HLS default Context to Streaming; ordinary audio defaults to Static. The registered Jellyfin nullable-enum binder leaves invalid text unset, accepts undefined Int32 values, and combines comma-separated names/numbers with bitwise OR. A retained actual .NET10 EnumConverter oracle covers limits, whitespace and trailing NUL behavior. Fragmented MP4 flags require both MP4 and Streaming context. HEAD resolves the format without starting an encoder. GET has no fixed length or range processing, waits for appended bytes while the producing encoder lives, and ends after it exits.

Native identifier limits prevent Container/codec values from escaping the captured output root. Container inference follows shifted target codecs before copy selection; audio inference uses the request URL suffix. Positive progressive seek clamps to runtime minus five seconds while progress retains the requested offset. Effective EncoderPath/EncoderProtocol require both fields. Ordinary native-rate input uses -re except RTSP and adds catchup on FFmpeg 8+. Earlier live roots, nullable CPU binding, preset and HEVC/AV1 selection remain intact. Advanced sample-rate/VBR/subtitle-embed/CopyTimestamps binding remains open review scope; segment deletion and mux queue settings belong to subsequent findings.

Regression checks cover policy boundaries, actual progress parsing, live configuration changes, failed writes, teardown ordering, replaced-job response ownership, dropped/error responses, growing files, nullable Context, permissions and container inference. Real FFmpeg integration checks stderr, interactive help and graceful exit. Native HTTP validation separately labels stock FFmpeg capability and a controlled wrapper: the latter delegates real encoding/statistics to FFmpeg, inserts fixture-only readrate 10 and a 120-second initial read burst and implements observed p/u with SIGSTOP/SIGCONT. This verifies controller transport and actual paused/running output; it does not certify stock FFmpeg p/u or hardware acceleration. The aborted-HLS probe repeats actual TS packets into a bounded response solely to exceed socket buffering; those bytes never supply encoder statistics.

The first API gate exposed scalar Container metadata on universal audio and an anonymous fixture using an always-authenticated stub. Route-specific collection metadata now preserves ordinary scalar binding and the existing universal fallback; the anonymous fixture supplies genuinely unauthenticated authorization. Universal-audio container negotiation remains an open behavior gap and is not completed by this throttling finding. The day-qualified progress parser correction also preserves its existing regression. Failed attempts are retained.

The initial prepared enum binder incorrectly assumed default ASP.NET rejection rules. Native reference Context=2 returned 200; pinned NullableEnumModelBinder and its provider registration instead use EnumConverter and catch FormatException, leaving invalid text unset. Production binding and a committed actual-oracle fixture now follow that source. Framework conversion source: [EnumConverter at .NET10](https://github.com/dotnet/runtime/blob/v10.0.0/src/libraries/System.ComponentModel.TypeConverter/src/System/ComponentModel/EnumConverter.cs).

The initial aborted-response fixture omitted the stream User-Agent, which the pinned output-path hash includes, and therefore triggered a reference seek/replacement. Its apparent resume was rejected by the independent exact producing-job count. Normal and aborted requests now carry the same explicit User-Agent, with the raw aborted request retained. The cached segment receives a deliberately advanced client marker of RuntimeTicks=600 seconds plus ActualSegmentLengthTicks=6 seconds; this tests callback ownership and cancellation, not actual playback of 606 seconds. Real output/progress and exact child identities remain independently verified.

Validation passed:

- fmt: passed (2.36 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (39.71 s, including any compilation).
- build: passed (2.63 s, including any compilation).
- native-before: passed (84.97 s, including any compilation).
- native-reference: passed (133.16 s, including any compilation).
- native-after: passed (126.09 s, including any compilation).
- clippy: passed (1.61 s, including any compilation).
- doctests: 3 passed, 0 ignored (6.57 s, including any compilation).
- ferrofin-mediaencoding: 650 tests run: 650 passed, 288 skipped (17.34 s, including any compilation).
- ferrofin-hls: 32 tests run: 32 passed, 85 skipped (2.39 s, including any compilation).
- ferrofin-api: 61 tests run: 61 passed, 866 skipped (69.39 s, including any compilation).
- ferrofin-model: 16 tests run: 16 passed, 965 skipped (30.42 s, including any compilation).
- server: 128 tests run: 128 passed, 51 skipped (7.30 s, including any compilation).
- traits: 76 tests run: 76 passed, 0 skipped (11.03 s, including any compilation).
- real-segment_transcode_ffmpeg: 8 tests run: 8 passed, 0 skipped (8.90 s, including any compilation).
- real-hls_stream_manager_ffmpeg: 4 tests run: 4 passed, 0 skipped (1.43 s, including any compilation).

Separate changed-crate line coverage:

- ferrofin-mediaencoding: 19,032/20,144 lines (94.479746%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 9 functions have mismatched data.
- ferrofin-hls: 3,066/3,300 lines (92.909091%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 1 functions have mismatched data.
- ferrofin-api: 42,531/49,204 lines (86.438094%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 53 functions have mismatched data.
- ferrofin-model: 8,954/9,921 lines (90.252999%), fresh full suite without seed.

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| stock / hls / context_binding / Videos / Context='' / response_ms | 2.037 | 2.390 | 36.131 |
| stock / hls / context_binding / Videos / Context=' \t' / response_ms | 2.462 | 2.239 | 9.574 |
| stock / hls / context_binding / Videos / Context='stReaMing' / response_ms | 2.424 | 2.190 | 7.620 |
| stock / hls / context_binding / Videos / Context='STATIC' / response_ms | 1.635 | 1.929 | 7.047 |
| stock / hls / context_binding / Videos / Context='0' / response_ms | 1.648 | 1.700 | 6.949 |
| stock / hls / context_binding / Videos / Context='1' / response_ms | 1.716 | 1.769 | 6.965 |
| stock / hls / context_binding / Videos / Context='2' / response_ms | 1.644 | 1.714 | 6.762 |
| stock / hls / context_binding / Videos / Context='other' / response_ms | 1.487 | 1.679 | 6.879 |
| stock / hls / context_binding / Audio / Context='' / response_ms | 1.396 | 1.979 | 15.848 |
| stock / hls / context_binding / Audio / Context=' \t' / response_ms | 1.311 | 1.957 | 8.116 |
| stock / hls / context_binding / Audio / Context='stReaMing' / response_ms | 1.181 | 1.845 | 7.731 |
| stock / hls / context_binding / Audio / Context='STATIC' / response_ms | 1.236 | 1.941 | 6.890 |
| stock / hls / context_binding / Audio / Context='0' / response_ms | 1.071 | 2.007 | 6.956 |
| stock / hls / context_binding / Audio / Context='1' / response_ms | 1.318 | 1.830 | 7.235 |
| stock / hls / context_binding / Audio / Context='2' / response_ms | 1.262 | 1.792 | 6.882 |
| stock / hls / context_binding / Audio / Context='other' / response_ms | 1.623 | 1.770 | 7.266 |
| stock / hls / first_response / response_ms | 92.381 | 86.209 | 238.124 |
| stock / progressive / context_binding / Videos / Context='' / response_ms | 4.121 | 2.828 | 34.177 |
| stock / progressive / context_binding / Videos / Context=' \t' / response_ms | 4.729 | 2.678 | 9.079 |
| stock / progressive / context_binding / Videos / Context='stReaMing' / response_ms | 4.148 | 2.037 | 7.279 |
| stock / progressive / context_binding / Videos / Context='STATIC' / response_ms | 4.257 | 2.309 | 6.931 |
| stock / progressive / context_binding / Videos / Context='0' / response_ms | 2.804 | 1.982 | 6.718 |
| stock / progressive / context_binding / Videos / Context='1' / response_ms | 2.272 | 2.028 | 6.641 |
| stock / progressive / context_binding / Videos / Context='2' / response_ms | 2.016 | 1.891 | 6.469 |
| stock / progressive / context_binding / Videos / Context='other' / response_ms | 2.157 | 1.982 | 6.584 |
| stock / progressive / context_binding / Audio / Context='' / response_ms | 1.676 | 1.773 | 14.875 |
| stock / progressive / context_binding / Audio / Context=' \t' / response_ms | 1.393 | 1.652 | 7.717 |
| stock / progressive / context_binding / Audio / Context='stReaMing' / response_ms | 1.356 | 1.649 | 7.444 |
| stock / progressive / context_binding / Audio / Context='STATIC' / response_ms | 1.364 | 1.808 | 6.650 |
| stock / progressive / context_binding / Audio / Context='0' / response_ms | 1.647 | 1.749 | 7.077 |
| stock / progressive / context_binding / Audio / Context='1' / response_ms | 1.836 | 1.859 | 7.358 |
| stock / progressive / context_binding / Audio / Context='2' / response_ms | 1.286 | 1.725 | 6.735 |
| stock / progressive / context_binding / Audio / Context='other' / response_ms | 1.287 | 1.767 | 6.887 |
| stock / progressive / first_response / response_ms | 1.343 | 1028.297 | 1124.476 |
| patched-wrapper / hls / context_binding / Videos / Context='' / response_ms | 2.429 | 3.291 | 39.632 |
| patched-wrapper / hls / context_binding / Videos / Context=' \t' / response_ms | 2.995 | 2.591 | 13.488 |
| patched-wrapper / hls / context_binding / Videos / Context='stReaMing' / response_ms | 2.533 | 2.265 | 11.493 |
| patched-wrapper / hls / context_binding / Videos / Context='STATIC' / response_ms | 1.702 | 2.781 | 8.744 |
| patched-wrapper / hls / context_binding / Videos / Context='0' / response_ms | 1.526 | 2.223 | 7.974 |
| patched-wrapper / hls / context_binding / Videos / Context='1' / response_ms | 1.408 | 2.058 | 7.769 |
| patched-wrapper / hls / context_binding / Videos / Context='2' / response_ms | 1.673 | 1.879 | 7.696 |
| patched-wrapper / hls / context_binding / Videos / Context='other' / response_ms | 1.664 | 2.029 | 7.675 |
| patched-wrapper / hls / context_binding / Audio / Context='' / response_ms | 1.935 | 2.312 | 17.684 |
| patched-wrapper / hls / context_binding / Audio / Context=' \t' / response_ms | 2.109 | 2.227 | 9.121 |
| patched-wrapper / hls / context_binding / Audio / Context='stReaMing' / response_ms | 2.234 | 1.945 | 8.065 |
| patched-wrapper / hls / context_binding / Audio / Context='STATIC' / response_ms | 2.015 | 2.131 | 7.700 |
| patched-wrapper / hls / context_binding / Audio / Context='0' / response_ms | 2.134 | 2.133 | 7.553 |
| patched-wrapper / hls / context_binding / Audio / Context='1' / response_ms | 2.055 | 1.834 | 7.314 |
| patched-wrapper / hls / context_binding / Audio / Context='2' / response_ms | 2.217 | 1.963 | 8.077 |
| patched-wrapper / hls / context_binding / Audio / Context='other' / response_ms | 2.233 | 1.866 | 7.694 |
| patched-wrapper / hls / first_response / response_ms | 121.524 | 106.870 | 238.321 |
| patched-wrapper / progressive / context_binding / Videos / Context='' / response_ms | 2.481 | 2.595 | 33.626 |
| patched-wrapper / progressive / context_binding / Videos / Context=' \t' / response_ms | 2.392 | 2.373 | 9.255 |
| patched-wrapper / progressive / context_binding / Videos / Context='stReaMing' / response_ms | 7.909 | 2.370 | 7.165 |
| patched-wrapper / progressive / context_binding / Videos / Context='STATIC' / response_ms | 3.905 | 1.813 | 6.547 |
| patched-wrapper / progressive / context_binding / Videos / Context='0' / response_ms | 3.794 | 1.867 | 6.442 |
| patched-wrapper / progressive / context_binding / Videos / Context='1' / response_ms | 3.137 | 1.710 | 6.430 |
| patched-wrapper / progressive / context_binding / Videos / Context='2' / response_ms | 2.317 | 1.994 | 6.309 |
| patched-wrapper / progressive / context_binding / Videos / Context='other' / response_ms | 3.402 | 1.902 | 6.487 |
| patched-wrapper / progressive / context_binding / Audio / Context='' / response_ms | 2.661 | 1.858 | 14.294 |
| patched-wrapper / progressive / context_binding / Audio / Context=' \t' / response_ms | 2.671 | 1.769 | 7.333 |
| patched-wrapper / progressive / context_binding / Audio / Context='stReaMing' / response_ms | 2.059 | 1.703 | 7.121 |
| patched-wrapper / progressive / context_binding / Audio / Context='STATIC' / response_ms | 1.598 | 1.716 | 6.604 |
| patched-wrapper / progressive / context_binding / Audio / Context='0' / response_ms | 1.408 | 1.794 | 6.346 |
| patched-wrapper / progressive / context_binding / Audio / Context='1' / response_ms | 1.683 | 1.745 | 6.352 |
| patched-wrapper / progressive / context_binding / Audio / Context='2' / response_ms | 1.469 | 2.137 | 6.321 |
| patched-wrapper / progressive / context_binding / Audio / Context='other' / response_ms | 1.380 | 1.989 | 6.506 |
| patched-wrapper / progressive / first_response / response_ms | 1.925 | 1046.089 | 1122.685 |
| patched-wrapper / hls / enable_clamped_delay / wait_ms | unavailable | 4112.218 | 3909.406 |
| patched-wrapper / hls / raise_delay_live / wait_ms | unavailable | 4213.165 | 4212.662 |
| patched-wrapper / hls / reduce_delay_live / wait_ms | unavailable | 3009.516 | 3006.055 |
| patched-wrapper / hls / disable_live / wait_ms | unavailable | 5016.304 | 5016.785 |
| patched-wrapper / hls / reenable_live / wait_ms | unavailable | 4916.674 | 4910.297 |
| patched-wrapper / hls / stop_encoding / stop_ms | unavailable | 1763.046 | 1765.297 |
| patched-wrapper / hls / cancellation_job_first_response / response_ms | unavailable | 107.038 | 212.663 |
| patched-wrapper / hls / cancellation_job_pause / wait_ms | unavailable | 5211.626 | 9937.786 |
| patched-wrapper / hls / aborted_hls_callback / wait_ms | unavailable | 5018.508 | 5017.369 |
| patched-wrapper / progressive / enable_clamped_delay / wait_ms | unavailable | 4112.155 | 4012.459 |
| patched-wrapper / progressive / raise_delay_live / wait_ms | unavailable | 4313.496 | 4211.446 |
| patched-wrapper / progressive / reduce_delay_live / wait_ms | unavailable | 3010.209 | 3009.310 |
| patched-wrapper / progressive / disable_live / wait_ms | unavailable | 4919.942 | 5015.697 |
| patched-wrapper / progressive / reenable_live / wait_ms | unavailable | 5019.316 | 4916.656 |
| patched-wrapper / progressive / stop_encoding / stop_ms | unavailable | 3800.394 | 1721.489 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t06/checks.json` and `/tmp/ferrofin-next10-root/evidence/t06/coverage.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.


Final complete workspace matrix: **8,393 passed, 5 skipped, 1 slow**, across all 21 workspace packages and 201 default test targets. Every package has a passing run with real FFmpeg enabled; WASM guest tests were explicitly disabled. Generated FFmpeg logs were preserved byte-for-byte, and only generated test executables were cleaned between packages. Full commands, target enumeration and frozen source evidence: `/tmp/ferrofin-next10-root/evidence/final88-matrix/run-002`. The previous passing formatting, strict all-target/all-feature Clippy and three-doctest checks match all 896 current Rust/Cargo source hashes; proof: `/tmp/ferrofin-next10-root/final88-quality-source-proof-v2.json`. The actual nullable Context oracle fixture is independently hashed in both this proof and the complete matrix.

The initial full server suite caught two pre-existing direct-play fixtures that omitted `Static=true` while expecting original-file bytes/ranges. Pinned `VideosController` only serves that static path when the flag is true; otherwise it starts progressive transcoding. Both fixture requests now explicitly select static serving. Their status/range/body assertions remain unchanged. All 227 server tests were rerun, while the 20 successful packages were retained with proof that only these server integration fixture files changed. The failed run remains in `run-001`.
