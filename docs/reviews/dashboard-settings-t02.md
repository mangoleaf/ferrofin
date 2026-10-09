# T02 — effective transcode directory (`TranscodingTempPath`)

The saved encoding path now selects the one live application transcode root. Startup loads the saved value; committed named-config changes publish it to subsequent operations. Output planning, current-root legacy HLS routes, system/storage reporting, maintenance cleanup and Live TV buffer selection use that shared root and prepare its ownership marker. An already planned operation retains its original path; this selects destinations rather than moving existing files.

Primary source pin `4910aafa1a` uses `EncodingConfigurationExtensions.GetTranscodePath`, recursive marker validation in `BaseApplicationPaths`, `SystemManager` and legacy HLS/transcode/cleanup consumers. Web pin is `1e507c588f353482a00333f84e36ddb7c8fc8221`. Null or empty values restore the current cache/transcodes default, and whitespace remains a configured path. The original audit's validator warning is stale: C02 previously registered the encoding-store validator in production. Changed non-whitespace paths must exist before saving; unchanged paths and whitespace follow the source's distinct handling. A rejected save must preserve both persisted and effective selection.

Core regressions cover startup, committed updates, captured operation paths, cache fallback, restart and consistent system/storage reports. Owned utility tests cover recursive matching/conflicting markers, case-sensitive Linux wildcard prefix, dangling conflicting markers and directory/marker creation errors. The native fixture saves two actual directories and fetches generated HLS playlist/segment bytes in each, exercises rejection and legacy filename routing, restarts, clears the setting, saves whitespace and observes conflicting marker errors. Eight additional phases cover nested matching-case markers, ignored uppercase glob prefix, dangling conflicting markers, a directory in place of the marker, preservation of existing marker bytes, and matching dangling links, FIFOs and sockets. Special entries retain their exact lstat identity; no FIFO/socket payload is opened. Linux socket binding uses an owned directory descriptor to avoid long socket addresses and process-wide cwd changes.

Separate changed-crate coverage is required for utility, core, HLS and Live TV. Traits and server wiring are exempt from line coverage but compile, planner/composition and real HTTP checks remain required. The new native fixture exercises output and storage paths; it does not claim to run a real tuner, scheduled cleanup task, or a path switch while a previous transcode remains running. Their shared-root/captured-path wiring is source and unit backed. Broader marker ownership and filesystem enumeration edges remain separate; a cyclic symlink fixture is not executed.

The first two core attempts and the following server build were stopped during compilation when host free space crossed the original 416 GiB reserve. Their logs and failure records remain intact; none reached a failing test. Quiescent cleanup removed generated test executables, coverage build cache and an obsolete generated release cache, retaining source, commits, actual server binaries, response bytes and exported profiles. The validator now records budget failures without triggering a second guard exception while recording them.

A read-only request to this node's kubelet `configz` verified explicit `nodefs.available` and `imagefs.available` hard thresholds of `100Gi`, with no soft thresholds. The root-only validation reserve is consequently recalibrated to 256 GiB, leaving 156 GiB above the configured threshold and keeping the 30 GiB generated-target cap, one compiler job and watched process groups. No node, cluster or historical helper file is changed. [Kubernetes documents absolute eviction thresholds](https://kubernetes.io/docs/concepts/scheduling-eviction/node-pressure-eviction/). The live observation, its hash and the policy are preserved in `/tmp/ferrofin-next10-root/node-disk-policy-probe.json` and `budget-policy.json`; all earlier guard interruptions remain failed attempts.

Earlier native before/reference, utility, core, HLS and Live TV passes use manifests 001/002. Manifest 003 changes the root budget/evidence policy and adds a scoped deprecated-field allowance to the test that deliberately verifies both legacy `SystemInfo.TranscodingTempPath` and newer storage reporting. Production Rust and native fixtures are unchanged. The final full core coverage suite and strict lint exercise that exact test. The first server, HTTP, binary and 16-phase native-after gates passed on manifest 003, but its lint gate rejected the load-function length and missing warning semicolon. A private initializer extraction preserves the exact cache→encoding/default/path preparation→metadata startup order, without suppression. Independently reviewed Unix marker creation now mirrors .NET LStat/Stat existence rules: matching dangling symlinks and non-directory special files are already present; matching directories and directory links still fail. The original regular-file-only check could create a dangling link target or block on a FIFO. The subsequent core coverage compilation was intentionally stopped; its raw profiles and the preliminary utility export remain retained. The validation runner now requires passing normal gates before dependent coverage and gives every verified profile an immutable attempt-specific path. Manifest 005 freezes these source/fixture corrections. Utility/core/server/HTTP/build, expanded 19-phase native runs, lint/doc and fresh full coverage use it; earlier targeted HLS/Live TV passes are retained alongside exact final full suites.

Validation passed:

- fmt: passed (2.29 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (28.36 s, including any compilation).
- build: passed (63.02 s, including any compilation).
- native-before: passed (8.94 s, including any compilation).
- native-reference: passed (13.93 s, including any compilation).
- native-after: passed (8.86 s, including any compilation).
- clippy: passed (121.18 s, including any compilation).
- doctests: 3 passed, 0 ignored (77.17 s, including any compilation).
- ferrofin-util: 10 tests run: 10 passed, 194 skipped (2.03 s, including any compilation).
- ferrofin-core: 62 tests run: 62 passed, 2351 skipped (110.43 s, including any compilation).
- ferrofin-hls: 31 tests run: 31 passed, 85 skipped (40.59 s, including any compilation).
- ferrofin-livetv: 140 tests run: 140 passed, 156 skipped (67.41 s, including any compilation).
- server: 74 tests run: 74 passed, 95 skipped (81.39 s, including any compilation).

Separate changed-crate line coverage:

- ferrofin-util: 2,218/2,291 lines (96.813619%), fresh full suite without seed.
- ferrofin-core: 118,304/124,021 lines (95.390297%), fresh full suite without seed.
- ferrofin-hls: 2,800/3,232 lines (86.633663%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 1 functions have mismatched data.
- ferrofin-livetv: 10,324/11,000 lines (93.854545%), fresh full suite without seed.

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| default_path / encoding.milliseconds | 2.282 | 2.286 | 48.115 |
| default_path / info.milliseconds | 2.502 | 1.686 | 8.657 |
| default_path / storage.milliseconds | 3.124 | 1.715 | 8.851 |
| default_path / startup.startup_ms | 4935.517 | 4897.239 | 5952.164 |
| fixture_media_main / generation_ms | 43.277 | 44.153 | 40.504 |
| configured_A / encoding.milliseconds | 1.605 | 0.815 | 5.972 |
| configured_A / info.milliseconds | 1.821 | 1.395 | 6.088 |
| configured_A / storage.milliseconds | 2.561 | 2.213 | 8.256 |
| configured_A / save.milliseconds | 7.017 | 7.117 | 13.772 |
| transcode_A / encoding.milliseconds | 1.573 | 1.492 | 6.643 |
| transcode_A / info.milliseconds | 1.791 | 1.786 | 5.942 |
| transcode_A / storage.milliseconds | 3.081 | 2.646 | 6.754 |
| transcode_A / transcode.playlist.milliseconds | 3.466 | 2.836 | 40.250 |
| transcode_A / transcode.segment.milliseconds | 42.403 | 43.640 | 141.466 |
| configured_B / encoding.milliseconds | 0.923 | 2.382 | 5.857 |
| configured_B / info.milliseconds | 1.361 | 2.262 | 5.801 |
| configured_B / storage.milliseconds | 2.663 | 3.115 | 7.884 |
| configured_B / save.milliseconds | 6.340 | 7.464 | 6.222 |
| transcode_B / encoding.milliseconds | 1.461 | 1.923 | 7.341 |
| transcode_B / info.milliseconds | 1.589 | 2.263 | 5.982 |
| transcode_B / storage.milliseconds | 2.511 | 4.100 | 6.751 |
| transcode_B / transcode.playlist.milliseconds | 3.285 | 2.990 | 13.187 |
| transcode_B / transcode.segment.milliseconds | 40.088 | 37.422 | 114.673 |
| missing_changed_path_rejected / encoding.milliseconds | 0.840 | 0.785 | 5.832 |
| missing_changed_path_rejected / info.milliseconds | 1.394 | 1.319 | 6.387 |
| missing_changed_path_rejected / storage.milliseconds | 3.243 | 2.377 | 6.794 |
| missing_changed_path_rejected / save.milliseconds | 1.272 | 1.567 | 7.667 |
| legacy_current_root / encoding.milliseconds | 1.753 | 0.710 | 6.253 |
| legacy_current_root / info.milliseconds | 1.887 | 1.104 | 5.954 |
| legacy_current_root / storage.milliseconds | 3.250 | 1.948 | 6.409 |
| legacy_current_root / legacy.milliseconds | 0.882 | 0.896 | 13.082 |
| saved_path_after_restart / encoding.milliseconds | 1.645 | 1.911 | 82.939 |
| saved_path_after_restart / info.milliseconds | 1.698 | 1.683 | 12.545 |
| saved_path_after_restart / storage.milliseconds | 2.733 | 2.531 | 13.175 |
| saved_path_after_restart / startup.startup_ms | 2699.511 | 2680.466 | 4532.456 |
| clear_to_cache / encoding.milliseconds | 0.763 | 1.979 | 9.892 |
| clear_to_cache / info.milliseconds | 1.094 | 2.250 | 7.037 |
| clear_to_cache / storage.milliseconds | 3.181 | 2.930 | 6.832 |
| clear_to_cache / save.milliseconds | 6.856 | 7.654 | 22.895 |
| whitespace_is_a_path / encoding.milliseconds | 0.706 | 0.810 | 5.531 |
| whitespace_is_a_path / info.milliseconds | 0.930 | 1.265 | 5.940 |
| whitespace_is_a_path / storage.milliseconds | 1.711 | 1.926 | 7.724 |
| whitespace_is_a_path / save.milliseconds | 6.357 | 6.588 | 6.060 |
| conflict_detected_on_access / encoding.milliseconds | 0.641 | 0.675 | 7.166 |
| conflict_detected_on_access / info.milliseconds | 0.883 | 1.287 | 10.736 |
| conflict_detected_on_access / storage.milliseconds | 1.549 | 1.304 | 6.621 |
| conflict_detected_on_access / save.milliseconds | 5.720 | 5.331 | 8.519 |
| marker_matching_nested / save.milliseconds | 6.266 | 7.626 | 6.547 |
| marker_matching_nested / encoding.milliseconds | 0.654 | 0.733 | 32.360 |
| marker_matching_nested / info.milliseconds | 0.915 | 1.029 | 11.998 |
| marker_matching_nested / storage.milliseconds | 1.636 | 1.784 | 12.514 |
| marker_uppercase_prefix / save.milliseconds | 5.913 | 7.147 | 6.103 |
| marker_uppercase_prefix / encoding.milliseconds | 0.879 | 2.758 | 5.495 |
| marker_uppercase_prefix / info.milliseconds | 1.225 | 2.866 | 5.797 |
| marker_uppercase_prefix / storage.milliseconds | 2.675 | 4.387 | 7.017 |
| marker_dangling_conflict / save.milliseconds | 7.065 | 7.197 | 6.283 |
| marker_dangling_conflict / encoding.milliseconds | 0.951 | 0.841 | 6.151 |
| marker_dangling_conflict / info.milliseconds | 1.473 | 1.473 | 6.124 |
| marker_dangling_conflict / storage.milliseconds | 1.984 | 1.466 | 6.171 |
| marker_is_directory / save.milliseconds | 5.907 | 5.923 | 5.769 |
| marker_is_directory / encoding.milliseconds | 0.670 | 0.657 | 8.779 |
| marker_is_directory / info.milliseconds | 0.960 | 1.152 | 12.723 |
| marker_is_directory / storage.milliseconds | 1.622 | 1.247 | 6.625 |
| marker_existing_not_truncated / save.milliseconds | 6.516 | 5.463 | 31.995 |
| marker_existing_not_truncated / encoding.milliseconds | 0.714 | 0.662 | 6.742 |
| marker_existing_not_truncated / info.milliseconds | 1.063 | 1.064 | 18.778 |
| marker_existing_not_truncated / storage.milliseconds | 2.151 | 2.075 | 6.900 |
| marker_matching_dangling / save.milliseconds | 6.015 | 7.506 | 11.216 |
| marker_matching_dangling / encoding.milliseconds | 0.858 | 1.500 | 10.466 |
| marker_matching_dangling / info.milliseconds | 1.052 | 1.745 | 6.675 |
| marker_matching_dangling / storage.milliseconds | 2.205 | 2.750 | 6.741 |
| marker_matching_fifo / save.milliseconds | 6.561 | 5.484 | 6.776 |
| marker_matching_fifo / encoding.milliseconds | 0.799 | 0.717 | 8.154 |
| marker_matching_fifo / info.milliseconds | 1.009 | 1.083 | 5.866 |
| marker_matching_fifo / storage.milliseconds | 1.679 | 1.902 | 6.301 |
| marker_matching_socket / save.milliseconds | 5.813 | 7.008 | 5.751 |
| marker_matching_socket / encoding.milliseconds | 0.653 | 0.650 | 5.251 |
| marker_matching_socket / info.milliseconds | 0.899 | 0.956 | 5.373 |
| marker_matching_socket / storage.milliseconds | 1.805 | 1.700 | 6.789 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t02/checks.json` and `/tmp/ferrofin-next10-root/evidence/t02/coverage.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.


Independent source and actual-native acceptance is preserved in `/tmp/ferrofin-next10-throttling/t02-independent-review/source005-native-acceptance.json` (SHA256 `f406c37297ac5713c069dede38f17f20e07db60dfddb5c9ccbb8942c07b4e3e8`). Its receipt preceded the now-passing final normal/coverage gates; it does not substitute for them.

The HLS coverage suite includes opt-in FFmpeg cases that return early when their environment flag is absent. The separate actual-binary fixture executes real FFmpeg HLS jobs and verifies the resulting bytes; no opt-in integration execution is inferred from the coverage test count.
