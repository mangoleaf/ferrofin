# T03 — encoding threads and `CpuCoreLimit`

The missing `CpuCoreLimit` query now reaches the shared streaming request and planner. It overrides saved `EncodingThreadCount` before the existing helper applies the host processor count; it is not a minimum against the saved setting. Nonpositive selected values emit FFmpeg `-threads 0`. Positive values are capped at the processor count captured when `EncodingHelper` is built.

Primary source is Jellyfin `4910aafa1a`, `EncodingHelper.GetNumberOfThreads` and the nullable CPU request binding; web pin is `1e507c588f353482a00333f84e36ddb7c8fc8221`. A CPU-specific deserializer follows ASP.NET Core v10 `SimpleTypeModelBinder` and .NET `BaseNumberConverter` / `Int32Converter`: omitted, bare, empty and whitespace values are null; nonempty values are trimmed before signed decimal or #/0x/&h hexadecimal conversion, including signed bit patterns and the decimal trailing-NUL compatibility. Malformed/overflow/fraction/interior-NUL values reject before planning. Only this query field uses the new helper. Existing named JSON binding, raw-response immutability and earlier path/display findings remain intact.

Source-derived regressions use the actual query extractor and a planner with persisted named encoding configuration. They cover case spellings, blank/Unicode inputs, signed boundaries, hexadecimal forms, malformed values, override precedence and emitted arguments. Planner unit tests pin eight processors for determinism. Root separately measured the production helper's `std::thread::available_parallelism`:32 available and32 allowed processors, recorded in `/tmp/ferrofin-next10-root/cpu-count.json` with source and binary hashes. That current runtime evidence supports the native positive2/3 assertions; the processor-count detector is unchanged.

The native fixture has 31 substantive setting/request phases: the original seven thread-selection cases plus15 accepted/absent nullable-query forms and9 rejected forms. Bootstrap/media/probe/owned-cleanup rows are additional evidence. It saves settings over authenticated HTTP, captures immutable copies of actual encoding JSON/XML and raw response bytes/status/headers/timing, then receives a real HLS playlist/transport segment and the actual launched FFmpeg argv for accepted requests. Rejected inputs must return400 without starting an HLS encoder. The independent verifier checks saved file content, exact query, real TS packet bytes and `-threads`, without trusting a fixture pass label. Baseline accepts observed behavior differences while enforcing evidence integrity; reference/after enforce source-derived substantive outcomes.

The recording executable passes unchanged args to actual FFmpeg and restricts only that process's affinity; the server processor count remains the measured host count. This verifies argument propagation and genuine media output, not thread-performance scaling. Supporting Jellyfin12.1.0 runtime evidence remains separate from the primary pin; no GPU execution claim is made. 

Changed-crate coverage is required separately for `ferrofin-api` at>=80%; server/traits retain their documented compile/integration exemptions. Run HLS query/controller regressions, planner propagation and existing thread-helper tests, the maintained dashboard HTTP regression, strict formatting/Clippy/doctests/build, serial native before/reference/after, then fresh API coverage. Preserve the date/JSON oracle inputs and existing T01/T02 verifier guards. The first expanded server gate exposed a preexisting composition fixture using `/cache/transcodes`, which T02 now prepares. Its owned temporary paths are repaired without changing production. Earlier API and native before/reference passes are retained with unchanged code/fixtures; the corrected server gate and final coverage use the subsequent frozen inputs. The failed attempt remains preserved.

Validation passed:

- fmt: passed (2.32 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (26.76 s, including any compilation).
- build: passed (65.42 s, including any compilation).
- native-before: passed (8.12 s, including any compilation).
- native-reference: passed (14.54 s, including any compilation).
- native-after: passed (7.78 s, including any compilation).
- clippy: passed (108.28 s, including any compilation).
- doctests: 3 passed, 0 ignored (79.41 s, including any compilation).
- ferrofin-api: 24 tests run: 24 passed, 891 skipped (270.72 s, including any compilation).
- server: 88 tests run: 88 passed, 82 skipped (8.88 s, including any compilation).
- thread-helper: 3 tests run: 3 passed, 763 skipped (14.54 s, including any compilation).

Separate changed-crate line coverage:

- ferrofin-api: 41,829/48,484 lines (86.273822%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 53 functions have mismatched data.

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| startup / startup.startup_ms | 5136.886 | 5103.444 | 7248.050 |
| fixture_media_main / generation_ms | 46.526 | 47.251 | 100.320 |
| threads_0 / save.milliseconds | 6.672 | 7.871 | 21.551 |
| threads_0 / encoding.milliseconds | 0.826 | 1.466 | 10.677 |
| threads_0 / operation.playlist.milliseconds | 2.793 | 3.321 | 111.003 |
| threads_0 / operation.segment.milliseconds | 50.356 | 54.127 | 158.264 |
| threads_0 / stop.milliseconds | 2.264 | 1.986 | 34.433 |
| threads_1 / save.milliseconds | 8.567 | 6.545 | 5.139 |
| threads_1 / encoding.milliseconds | 1.270 | 1.176 | 4.836 |
| threads_1 / operation.playlist.milliseconds | 3.260 | 3.316 | 9.044 |
| threads_1 / operation.segment.milliseconds | 50.360 | 50.082 | 112.021 |
| threads_1 / stop.milliseconds | 1.987 | 1.805 | 6.433 |
| threads_2 / save.milliseconds | 6.651 | 7.727 | 7.320 |
| threads_2 / encoding.milliseconds | 0.724 | 1.060 | 4.595 |
| threads_2 / operation.playlist.milliseconds | 2.512 | 3.146 | 7.554 |
| threads_2 / operation.segment.milliseconds | 54.842 | 51.833 | 110.520 |
| threads_2 / stop.milliseconds | 1.884 | 1.812 | 5.691 |
| threads_3 / save.milliseconds | 6.789 | 6.583 | 5.302 |
| threads_3 / encoding.milliseconds | 1.321 | 0.806 | 4.786 |
| threads_3 / operation.playlist.milliseconds | 3.271 | 2.795 | 8.332 |
| threads_3 / operation.segment.milliseconds | 47.968 | 51.351 | 111.178 |
| threads_3 / stop.milliseconds | 1.892 | 1.796 | 5.532 |
| threads_4 / save.milliseconds | 7.982 | 7.380 | 5.818 |
| threads_4 / encoding.milliseconds | 1.364 | 0.875 | 4.788 |
| threads_4 / operation.playlist.milliseconds | 3.627 | 2.652 | 7.937 |
| threads_4 / operation.segment.milliseconds | 48.978 | 54.565 | 110.881 |
| threads_4 / stop.milliseconds | 2.020 | 2.702 | 5.879 |
| threads_5 / save.milliseconds | 6.511 | 8.546 | 5.144 |
| threads_5 / encoding.milliseconds | 0.706 | 1.682 | 20.998 |
| threads_5 / operation.playlist.milliseconds | 2.195 | 3.368 | 7.828 |
| threads_5 / operation.segment.milliseconds | 45.258 | 51.402 | 110.222 |
| threads_5 / stop.milliseconds | 1.038 | 1.776 | 5.281 |
| threads_6 / save.milliseconds | 7.017 | 6.014 | 4.675 |
| threads_6 / encoding.milliseconds | 0.704 | 0.759 | 5.099 |
| threads_6 / operation.playlist.milliseconds | 3.443 | 2.518 | 8.142 |
| threads_6 / operation.segment.milliseconds | 45.050 | 56.005 | 112.769 |
| threads_6 / stop.milliseconds | 1.610 | 1.957 | 5.211 |
| cpu_query_omitted / save.milliseconds | 7.728 | 9.098 | 4.765 |
| cpu_query_omitted / encoding.milliseconds | 0.938 | 2.454 | 4.476 |
| cpu_query_omitted / operation.playlist.milliseconds | 2.673 | 4.360 | 7.738 |
| cpu_query_omitted / operation.segment.milliseconds | 45.338 | 59.982 | 111.124 |
| cpu_query_omitted / stop.milliseconds | 1.864 | 1.928 | 5.564 |
| cpu_query_omitted / elapsed_ms | 61.665 | 80.591 | 140.438 |
| cpu_query_bare / save.milliseconds | 7.304 | 7.450 | 4.687 |
| cpu_query_bare / encoding.milliseconds | 1.048 | 0.898 | 4.680 |
| cpu_query_bare / operation.playlist.milliseconds | 2.993 | 2.854 | 7.338 |
| cpu_query_bare / operation.segment.milliseconds | 43.943 | 51.868 | 110.405 |
| cpu_query_bare / stop.milliseconds | 1.056 | 1.806 | 5.397 |
| cpu_query_bare / elapsed_ms | 58.887 | 67.214 | 139.476 |
| cpu_query_empty / save.milliseconds | 8.595 | 6.795 | 7.581 |
| cpu_query_empty / encoding.milliseconds | 0.706 | 1.060 | 6.344 |
| cpu_query_empty / operation.playlist.milliseconds | 2.612 | 3.676 | 7.609 |
| cpu_query_empty / operation.segment.milliseconds | 44.568 | 50.820 | 110.476 |
| cpu_query_empty / stop.milliseconds | 1.008 | 1.914 | 8.152 |
| cpu_query_empty / elapsed_ms | 59.273 | 66.913 | 147.721 |
| cpu_query_ascii_white / save.milliseconds | 6.313 | 6.838 | 5.142 |
| cpu_query_ascii_white / encoding.milliseconds | 0.697 | 0.854 | 4.941 |
| cpu_query_ascii_white / operation.playlist.milliseconds | 2.197 | 3.021 | 8.340 |
| cpu_query_ascii_white / operation.segment.milliseconds | 43.667 | 50.410 | 211.861 |
| cpu_query_ascii_white / stop.milliseconds | 1.170 | 1.715 | 37.352 |
| cpu_query_ascii_white / elapsed_ms | 56.243 | 65.018 | 275.957 |
| cpu_query_unicode_white / save.milliseconds | 8.617 | 7.425 | 5.055 |
| cpu_query_unicode_white / encoding.milliseconds | 0.680 | 0.870 | 5.292 |
| cpu_query_unicode_white / operation.playlist.milliseconds | 2.918 | 2.773 | 7.810 |
| cpu_query_unicode_white / operation.segment.milliseconds | 46.412 | 51.381 | 110.600 |
| cpu_query_unicode_white / stop.milliseconds | 1.883 | 1.687 | 5.123 |
| cpu_query_unicode_white / elapsed_ms | 63.003 | 66.534 | 142.907 |
| cpu_query_trimmed_plus / save.milliseconds | 10.404 | 7.063 | 5.213 |
| cpu_query_trimmed_plus / encoding.milliseconds | 0.885 | 1.313 | 6.194 |
| cpu_query_trimmed_plus / operation.playlist.milliseconds | 2.130 | 3.713 | 8.322 |
| cpu_query_trimmed_plus / operation.segment.milliseconds | 45.052 | 52.627 | 115.390 |
| cpu_query_trimmed_plus / stop.milliseconds | 0.935 | 1.909 | 6.635 |
| cpu_query_trimmed_plus / elapsed_ms | 62.021 | 69.129 | 148.852 |
| cpu_query_unicode_trim / save.milliseconds | 5.567 | 7.558 | 4.865 |
| cpu_query_unicode_trim / encoding.milliseconds | 0.688 | 0.898 | 5.819 |
| cpu_query_unicode_trim / operation.playlist.milliseconds | 2.058 | 2.984 | 7.402 |
| cpu_query_unicode_trim / operation.segment.milliseconds | 43.919 | 51.965 | 185.796 |
| cpu_query_unicode_trim / stop.milliseconds | 1.631 | 1.576 | 5.668 |
| cpu_query_unicode_trim / elapsed_ms | 55.992 | 67.397 | 217.061 |
| cpu_query_hex / save.milliseconds | 7.684 | 7.171 | 7.854 |
| cpu_query_hex / encoding.milliseconds | 1.224 | 1.028 | 4.917 |
| cpu_query_hex / operation.playlist.milliseconds | 3.506 | 2.611 | 7.496 |
| cpu_query_hex / operation.segment.milliseconds | 45.358 | 55.147 | 110.481 |
| cpu_query_hex / stop.milliseconds | 1.840 | 1.764 | 5.442 |
| cpu_query_hex / elapsed_ms | 62.851 | 70.329 | 144.482 |
| cpu_query_hex_hash_nested_plus / save.milliseconds | 6.887 | 7.994 | 5.033 |
| cpu_query_hex_hash_nested_plus / encoding.milliseconds | 1.009 | 1.274 | 7.166 |
| cpu_query_hex_hash_nested_plus / operation.playlist.milliseconds | 2.912 | 5.023 | 9.202 |
| cpu_query_hex_hash_nested_plus / operation.segment.milliseconds | 45.320 | 63.266 | 110.652 |
| cpu_query_hex_hash_nested_plus / stop.milliseconds | 1.801 | 1.951 | 5.916 |
| cpu_query_hex_hash_nested_plus / elapsed_ms | 61.212 | 82.424 | 145.193 |
| cpu_query_hex_ampersand / save.milliseconds | 8.149 | 6.741 | 5.053 |
| cpu_query_hex_ampersand / encoding.milliseconds | 1.231 | 0.950 | 5.741 |
| cpu_query_hex_ampersand / operation.playlist.milliseconds | 4.729 | 3.234 | 12.738 |
| cpu_query_hex_ampersand / operation.segment.milliseconds | 50.023 | 63.021 | 114.971 |
| cpu_query_hex_ampersand / stop.milliseconds | 2.264 | 1.970 | 5.665 |
| cpu_query_hex_ampersand / elapsed_ms | 70.132 | 78.742 | 152.496 |
| cpu_query_hex_signed_bits / save.milliseconds | 7.409 | 6.937 | 5.462 |
| cpu_query_hex_signed_bits / encoding.milliseconds | 1.898 | 1.339 | 5.231 |
| cpu_query_hex_signed_bits / operation.playlist.milliseconds | 4.060 | 3.374 | 8.035 |
| cpu_query_hex_signed_bits / operation.segment.milliseconds | 50.013 | 53.326 | 111.379 |
| cpu_query_hex_signed_bits / stop.milliseconds | 1.893 | 2.152 | 6.376 |
| cpu_query_hex_signed_bits / elapsed_ms | 68.929 | 70.313 | 144.496 |
| cpu_query_trailing_nuls / save.milliseconds | 6.575 | 7.589 | 6.525 |
| cpu_query_trailing_nuls / encoding.milliseconds | 0.837 | 0.917 | 5.817 |
| cpu_query_trailing_nuls / operation.playlist.milliseconds | 3.022 | 3.396 | 8.898 |
| cpu_query_trailing_nuls / operation.segment.milliseconds | 53.471 | 52.266 | 112.144 |
| cpu_query_trailing_nuls / stop.milliseconds | 1.688 | 1.736 | 6.098 |
| cpu_query_trailing_nuls / elapsed_ms | 68.381 | 68.632 | 147.206 |
| cpu_query_white_before_nul / save.milliseconds | 7.442 | 15.104 | 5.570 |
| cpu_query_white_before_nul / encoding.milliseconds | 0.747 | 1.703 | 6.007 |
| cpu_query_white_before_nul / operation.playlist.milliseconds | 3.654 | 3.419 | 7.634 |
| cpu_query_white_before_nul / operation.segment.milliseconds | 46.621 | 54.671 | 113.720 |
| cpu_query_white_before_nul / stop.milliseconds | 1.526 | 1.702 | 7.739 |
| cpu_query_white_before_nul / elapsed_ms | 62.946 | 79.872 | 148.941 |
| cpu_query_duplicate_first / save.milliseconds | 6.579 | 7.430 | 30.825 |
| cpu_query_duplicate_first / encoding.milliseconds | 1.056 | 0.756 | 5.521 |
| cpu_query_duplicate_first / operation.playlist.milliseconds | 3.690 | 2.964 | 8.510 |
| cpu_query_duplicate_first / operation.segment.milliseconds | 50.975 | 56.401 | 222.907 |
| cpu_query_duplicate_first / stop.milliseconds | 2.112 | 2.062 | 7.830 |
| cpu_query_duplicate_first / elapsed_ms | 67.724 | 72.065 | 291.947 |
| cpu_query_duplicate_empty_first / save.milliseconds | 30.224 | 7.695 | 5.613 |
| cpu_query_duplicate_empty_first / encoding.milliseconds | 1.431 | 0.991 | 5.023 |
| cpu_query_duplicate_empty_first / operation.playlist.milliseconds | 3.700 | 3.428 | 7.812 |
| cpu_query_duplicate_empty_first / operation.segment.milliseconds | 54.742 | 50.384 | 467.531 |
| cpu_query_duplicate_empty_first / stop.milliseconds | 1.510 | 2.225 | 6.547 |
| cpu_query_duplicate_empty_first / elapsed_ms | 94.695 | 68.198 | 500.985 |
| cpu_query_invalid_word / save.milliseconds | 5.867 | 11.541 | 6.483 |
| cpu_query_invalid_word / encoding.milliseconds | 0.800 | 1.694 | 4.806 |
| cpu_query_invalid_word / operation.playlist.milliseconds | 2.400 | 1.135 | 11.795 |
| cpu_query_invalid_word / operation.segment.milliseconds | 47.143 | unavailable | unavailable |
| cpu_query_invalid_word / stop.milliseconds | 1.103 | 0.797 | 5.874 |
| cpu_query_invalid_word / elapsed_ms | 59.675 | 18.496 | 37.888 |
| cpu_query_decimal_overflow / save.milliseconds | 6.600 | 6.274 | 4.572 |
| cpu_query_decimal_overflow / encoding.milliseconds | 0.746 | 1.282 | 4.311 |
| cpu_query_decimal_overflow / operation.playlist.milliseconds | 2.651 | 0.986 | 4.891 |
| cpu_query_decimal_overflow / operation.segment.milliseconds | 46.984 | unavailable | unavailable |
| cpu_query_decimal_overflow / stop.milliseconds | 1.081 | 0.786 | 4.152 |
| cpu_query_decimal_overflow / elapsed_ms | 60.231 | 12.017 | 24.615 |
| cpu_query_hex_overflow / save.milliseconds | 6.750 | 6.782 | 4.535 |
| cpu_query_hex_overflow / encoding.milliseconds | 0.684 | 0.995 | 4.329 |
| cpu_query_hex_overflow / operation.playlist.milliseconds | 2.058 | 0.777 | 5.102 |
| cpu_query_hex_overflow / operation.segment.milliseconds | 46.941 | unavailable | unavailable |
| cpu_query_hex_overflow / stop.milliseconds | 1.014 | 0.612 | 4.078 |
| cpu_query_hex_overflow / elapsed_ms | 59.503 | 11.277 | 24.580 |
| cpu_query_fraction / save.milliseconds | 6.261 | 6.594 | 4.517 |
| cpu_query_fraction / encoding.milliseconds | 0.669 | 0.879 | 4.130 |
| cpu_query_fraction / operation.playlist.milliseconds | 2.225 | 0.622 | 4.676 |
| cpu_query_fraction / operation.segment.milliseconds | 44.144 | unavailable | unavailable |
| cpu_query_fraction / stop.milliseconds | 0.948 | 0.535 | 4.289 |
| cpu_query_fraction / elapsed_ms | 56.256 | 10.550 | 24.040 |
| cpu_query_negative_hex / save.milliseconds | 6.265 | 6.017 | 4.411 |
| cpu_query_negative_hex / encoding.milliseconds | 0.672 | 0.772 | 4.305 |
| cpu_query_negative_hex / operation.playlist.milliseconds | 2.289 | 0.666 | 5.008 |
| cpu_query_negative_hex / operation.segment.milliseconds | 43.328 | unavailable | unavailable |
| cpu_query_negative_hex / stop.milliseconds | 2.100 | 0.612 | 4.589 |
| cpu_query_negative_hex / elapsed_ms | 57.403 | 10.280 | 24.597 |
| cpu_query_double_hex_plus / save.milliseconds | 6.768 | 6.456 | 4.812 |
| cpu_query_double_hex_plus / encoding.milliseconds | 0.948 | 0.765 | 4.204 |
| cpu_query_double_hex_plus / operation.playlist.milliseconds | 2.661 | 0.683 | 4.641 |
| cpu_query_double_hex_plus / operation.segment.milliseconds | 53.102 | unavailable | unavailable |
| cpu_query_double_hex_plus / stop.milliseconds | 2.070 | 0.469 | 4.115 |
| cpu_query_double_hex_plus / elapsed_ms | 68.420 | 10.537 | 24.422 |
| cpu_query_interior_nul / save.milliseconds | 6.982 | 5.822 | 4.719 |
| cpu_query_interior_nul / encoding.milliseconds | 1.044 | 0.878 | 4.398 |
| cpu_query_interior_nul / operation.playlist.milliseconds | 3.211 | 0.748 | 4.815 |
| cpu_query_interior_nul / operation.segment.milliseconds | 54.864 | unavailable | unavailable |
| cpu_query_interior_nul / stop.milliseconds | 1.622 | 0.562 | 4.076 |
| cpu_query_interior_nul / elapsed_ms | 70.899 | 9.917 | 24.601 |
| cpu_query_unicode_before_nul / save.milliseconds | 7.425 | 8.408 | 4.587 |
| cpu_query_unicode_before_nul / encoding.milliseconds | 1.231 | 0.890 | 4.595 |
| cpu_query_unicode_before_nul / operation.playlist.milliseconds | 3.946 | 0.735 | 4.634 |
| cpu_query_unicode_before_nul / operation.segment.milliseconds | 54.265 | unavailable | unavailable |
| cpu_query_unicode_before_nul / stop.milliseconds | 2.019 | 0.597 | 4.175 |
| cpu_query_unicode_before_nul / elapsed_ms | 72.129 | 12.489 | 24.532 |
| cpu_query_only_nul / save.milliseconds | 7.921 | 6.428 | 4.565 |
| cpu_query_only_nul / encoding.milliseconds | 1.074 | 0.873 | 4.453 |
| cpu_query_only_nul / operation.playlist.milliseconds | 3.353 | 0.710 | 4.617 |
| cpu_query_only_nul / operation.segment.milliseconds | 50.435 | unavailable | unavailable |
| cpu_query_only_nul / stop.milliseconds | 1.724 | 0.489 | 4.369 |
| cpu_query_only_nul / elapsed_ms | 67.941 | 10.366 | 24.484 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t03/checks.json` and `/tmp/ferrofin-next10-root/evidence/t03/coverage.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.

