# N01 — Save selected-user watch data in NFO (`xbmcmetadata.UserId`)

The change completes the actual selected-user saver consumer. Source pin `4910aafa1a`, `BaseNfoSaver.cs:861–915`, exports the live configured user's rating/favorite/watched/count/date/resume fields, including hidden/disabled users. Blank Unicode whitespace and valid unknown users omit userdata. Source GUID forms D/N/B/P/X are accepted; malformed/URN/nil selections produce contained save failures and preserve the existing file.

Pinned `NfoUserDataSaver.cs:58–86` reacts only to PlaybackFinished, TogglePlayed and UpdateUserRating for eligible local metadata, dispatching Nfo with ordinary library saver policy. The weak-owned listener runs after user-data commit, initially polls inline and detaches pending work; saver failure cannot undo committed data. Automatic native/HTTP observations require actual file changes without a metadata edit. The native failure case holds the obstruction until a new matching saver-error receipt; an HTTP response alone does not prove detached I/O completed.

Likes writes map true→10, false→1 and null→no rating; the getter threshold is 6.5. The selected read now honors current provider/Series/channel/GUID key priority and exact attached-row matching. S22 active-DVR key production remains open. Normal pinned local NFO userdata import is inert: temporary-item mutations are neither merged nor persisted. The fixture proves an actual title import before asserting unchanged userdata.

Reviewed entrypoints:

```sh
python3 /tmp/ferrofin-dashboard-n01-checks.py --list
python3 /tmp/ferrofin-dashboard-n01-coverage.py --list
```

Core/API/providers each require an independent 80% gate; API uses a fresh full suite. Held/erroring listener and conflicting-key regressions complement the real mutation/file fixture. Jellyfin 12.1.0 comparison remains separate from the source pin; its actual results are recorded below. The owned wrapper bounds child cleanup and preserves original failures. Evidence and linked current manifests: `/tmp/ferrofin-n01-n02-independent-review/`. 

The first core run passed 158 tests and failed one hierarchy test before reaching its assertions. Its parent FK bind used lowercase `Uuid::to_string()`, while `seed_item` stores uppercase hyphenated IDs through `guid_to_db`. The correction uses that existing conversion for the parent bind, matching the neighboring cycle test. Production logic, schema, SQL text and all nearest-parent/missing-pointer/nil-pointer/hydrated/read/write assertions remain unchanged. The actual mismatch is casing, not hyphen removal. Original failure records are preserved under `/tmp/ferrofin-n01-parent-guid-fixture-followup/`; independent review is in `/tmp/ferrofin-n01-parent-guid-fixture-independent-review/`.

After the fixture correction, all 159 core cases passed. The API runner then failed before execution because `binary(playstate)` and `binary(session_ctx)` named handler modules rather than actual Cargo test targets. Cargo metadata confirms `user_library` and `session_playstate`; the corrected filter selects both actual integration binaries and retains the `test(user_library)`, `test(playstate)` and `test(session_ctx)` handler selectors. The API coverage gate still requires a fresh full suite without a seed. Failed-filter records and target metadata are preserved under `/tmp/ferrofin-n01-api-filter-review/` and `/tmp/ferrofin-n01-api-target-metadata.json`.

The providers seeded measurement failed the 80% gate at 1.50%. Its profiles/exports and the actual failed gate remain in the JSON attempt history. The automatic fallback ran the full current providers suite without a seed and measured 92.10%; its six-function LLVM diagnostic is retained. Core measured 82.14% with a verified seed and 22 mismatched functions. API measured 86.27% from the mandatory fresh full 908-test suite, retaining 53 mismatched functions. These diagnostics are disclosed below; no fresh successful run was repeated to erase them.

Validation passed:

- fmt: passed.
- core: 159 tests run: 159 passed, 2241 skipped.
- api: 77 tests run: 77 passed, 831 skipped.
- providers: 17 tests run: 17 passed, 847 skipped.
- traits: passed.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 101,420/123,465 lines (82.14%). Fresh affected tests and the recorded prior profile were merged and exported against current binaries. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 22 functions have mismatched data.
- ferrofin-api: 41,374/47,956 lines (86.27%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 53 functions have mismatched data.
- ferrofin-providers: 24,504/26,605 lines (92.10%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 6 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-n01-checks.json`,
`/tmp/ferrofin-dashboard-n01-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 23.55 GiB generated target, 518.08 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| selected_a / edit_ms | 20.850 | 19.840 | 78.960 |
| selected_hidden_disabled_b / edit_ms | 16.220 | 15.000 | 34.540 |
| selected_a_again / edit_ms | 22.990 | 14.470 | 28.360 |
| selected_whitespace_guid / edit_ms | 16.030 | 16.450 | 29.290 |
| selected_guid_n / edit_ms | 19.720 | 13.290 | 27.650 |
| selected_guid_b / edit_ms | 18.210 | 14.780 | 30.130 |
| selected_guid_p / edit_ms | 17.150 | 14.090 | 29.480 |
| selected_guid_x / edit_ms | 18.750 | 13.710 | 42.030 |
| malformed_selection / edit_ms | 18.600 | 7.910 | 24.400 |
| urn_selection / edit_ms | 18.400 | 8.250 | 24.860 |
| nil_selection / edit_ms | 21.570 | 7.940 | 24.040 |
| no_selection / edit_ms | 15.570 | 14.090 | 23.580 |
| whitespace_selection / edit_ms | 16.620 | 13.250 | 24.040 |
| unknown_selection / edit_ms | 17.090 | 13.710 | 25.240 |
| rating_boundary_5.0 / http_ms | 7.200 | 6.730 | 20.480 |
| rating_boundary_6.49 / http_ms | 5.160 | 4.880 | 18.290 |
| rating_boundary_6.5 / http_ms | 5.550 | 4.980 | 120.980 |
| auto_initial_favorite / http_ms | 2.720 | 2.720 | 16.450 |
| auto_initial_favorite / observation_ms | 5004.590 | 28.030 | 16.640 |
| ignored_update_userdata / http_ms | 6.690 | 3.270 | 8.910 |
| ignored_update_userdata / observation_ms | 6.760 | 3.320 | 8.960 |
| auto_favorite / http_ms | 6.360 | 3.050 | 14.500 |
| auto_favorite / observation_ms | 5017.690 | 28.290 | 14.630 |
| auto_like / http_ms | 4.720 | 3.380 | 13.930 |
| auto_like / observation_ms | 5020.510 | 28.640 | 14.090 |
| auto_dislike / http_ms | 5.950 | 3.320 | 12.620 |
| auto_dislike / observation_ms | 5006.090 | 28.580 | 13.720 |
| auto_clear_rating / http_ms | 5.730 | 3.810 | 26.480 |
| auto_clear_rating / observation_ms | 5019.010 | 29.060 | 26.650 |
| auto_played / http_ms | 5.940 | 3.270 | 38.320 |
| auto_played / observation_ms | 5030.450 | 28.510 | 38.470 |
| auto_unplayed / http_ms | 6.570 | 3.320 | 29.170 |
| auto_unplayed / observation_ms | 5027.240 | 28.580 | 29.330 |
| auto_other_user_exports_selected_b / http_ms | 4.140 | 3.460 | 12.290 |
| auto_other_user_exports_selected_b / observation_ms | 5005.280 | 28.820 | 12.410 |
| ignored_blank_selection / http_ms | 5.740 | 4.400 | 9.780 |
| ignored_blank_selection / observation_ms | 5.810 | 4.440 | 9.840 |
| ignored_disabled_saver / http_ms | 3.170 | 3.940 | 8.840 |
| ignored_disabled_saver / observation_ms | 3.200 | 3.980 | 8.880 |
| auto_saver_enabled_again / http_ms | 3.060 | 4.030 | 19.540 |
| auto_saver_enabled_again / observation_ms | 5024.310 | 29.380 | 19.830 |
| ignored_playback_start / http_ms | 2.970 | 3.400 | 35.000 |
| ignored_playback_start / observation_ms | 3.000 | 3.440 | 35.090 |
| ignored_playback_progress / http_ms | 1.820 | 2.920 | 17.700 |
| ignored_playback_progress / observation_ms | 1.850 | 2.960 | 17.770 |
| auto_playback_finished / http_ms | 2.040 | 3.260 | 27.820 |
| auto_playback_finished / observation_ms | 5014.940 | 28.650 | 28.030 |
| ignored_second_playback_start / http_ms | 5.710 | 4.090 | 19.660 |
| ignored_second_playback_start / observation_ms | 5.790 | 4.130 | 19.710 |
| auto_playback_finished_without_position / http_ms | 3.100 | 2.620 | 17.320 |
| auto_playback_finished_without_position / observation_ms | 5008.060 | 27.950 | 17.510 |
| userdata_survives_nfo_error / http_ms | 5.940 | 4.460 | 13.860 |
| userdata_survives_nfo_error / observation_ms | 5.970 | 4.470 | 13.880 |
| userdata_survives_nfo_error / saver_error_wait_ms | 5013.370 | 0.050 | 0.070 |
| normal_local_import / refresh_ms | 220.710 | 223.400 | 130.290 |
| auto_initial_favorite / consumer_ms | unavailable | 28.030 | 16.640 |
| auto_favorite / consumer_ms | unavailable | 28.290 | 14.630 |
| auto_like / consumer_ms | unavailable | 28.640 | 14.090 |
| auto_dislike / consumer_ms | unavailable | 28.580 | 13.720 |
| auto_clear_rating / consumer_ms | unavailable | 29.060 | 26.650 |
| auto_played / consumer_ms | unavailable | 28.510 | 38.470 |
| auto_unplayed / consumer_ms | unavailable | 28.580 | 29.330 |
| auto_other_user_exports_selected_b / consumer_ms | unavailable | 28.820 | 12.410 |
| auto_saver_enabled_again / consumer_ms | unavailable | 29.380 | 19.830 |
| auto_playback_finished / consumer_ms | unavailable | 28.650 | 28.030 |
| auto_playback_finished_without_position / consumer_ms | unavailable | 27.950 | 17.510 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.
