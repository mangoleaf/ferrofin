# M03 — Dummy chapter duration (`DummyChapterDuration`)

The change reads the live duration in the actual successful video-probe/scan flow. Source pin `4910aafa1a`, `FFProbeVideoInfo.cs:277–298,605–641`, generates dummy chapters only on Default/FullRefresh, with a positive interval, at most one original chapter and a Video stream. It produces at least one marker for a positive valid runtime, rejects negative or over-12-hour runtimes, permits exactly 12 hours, and uses localized generated names. Multiple original chapters survive.

Successful null-duration video probes now clear stale runtime before chapter construction. The source custom-provider exception path contains an ordinary chapter-construction error: fresh probe fields/streams and the normal completed refresh can persist, while old chapters/images survive and extraction is skipped. Cancellation remains distinct. This does not repair the independent general probe-failure fold.

Core regressions use actual item/stream/chapter repositories for corrupt and null runtimes. The native fixture uses real 36-second video, zero/negative/10/18/90-second settings, single-marker replacement, multiple-marker preservation and French generated names. Fresh Completed task history, Etag and persisted Video/runtime checks prevent stale observations. Jellyfin 12.1.0 supplies a separate runtime comparison; the actual results are recorded below. S26 blank/time-formatted original chapter-name normalization remains open.

Reviewed entrypoints:

```sh
python3 /tmp/ferrofin-m03-m04-independent-review/m03-checks.py --list
python3 /tmp/ferrofin-m03-m04-independent-review/m03-coverage.py --list
```

Canonical `/tmp/ferrofin-dashboard-m03-{checks,coverage}.py` aliases delegate to the reviewed chapter adapter. Core requires an independent 80% measurement with fresh affected tests and a verified seed or full-suite fallback. The native replacement bounds owned cleanup and preserves original errors. Evidence: `/tmp/ferrofin-m03-m04-independent-review/EVIDENCE.md`. The common locale HTTP gate is separate from the chapter-specific native evidence below.

## Actual failed runs and fixture corrections

The original Jellyfin 12.1.0 reference passed all eight duration/position controls but returned English generated names after saving `UICulture=fr`. A second run with explicit `culture=fr` requests still produced English names. The saved configuration and completed-task observations were correct. Pinned FFProbeVideoInfo calls `GetLocalizedString`, which reads ambient `CurrentUICulture`. The nested `LimitedConcurrencyLibraryScheduler` retains workers created by the initial English scan; its queued item carries no per-job execution context and idle workers retire after 60 seconds. The outer task's request culture therefore does not determine every persistent child worker's culture.

The revised fixture loads a fresh French `config/system.xml` before starting Jellyfin and keeps startup configuration, explicit refresh culture and saved-setting readbacks aligned. Every French-name, position, runtime, Etag, new Completed task, original-marker and image-off assertion remains. This prerequisite establishes French workers for the duration comparison; it does not claim live ambient-culture parity. S33 tracks distinct current/server string lookups, S34 the persistent nested-worker context, and S26 independent original-name normalization.

The first normal core run passed 73 cases and failed the two real-reprobe regressions because the reused extractor fixture seeded a Movie at fixed ID `0xC28`. The actual scanner derives Movie identity from kind/path, so the tests read the untouched old row after a different row was probed. The fixture now uses the same existing Movie identity derivation; seeded items, streams, chapter rows and image targets follow that ID. No production behavior or runtime/chapter/image/error assertion was changed to address these failures.

After the identity correction, 74 core cases passed and one failed: the fresh unknown runtime and dimensions persisted, but `DefaultVideoStreamIndex` remained the previous `2` instead of the probed `7` when metadata replacement was false. This was a production merge defect. The generic Data fill rules restored the old index after `apply_probe`. A guarded post-merge overlay now restores the successful Video probe's nullable index while preserving unrelated Data and existing Container behavior. Pinned ProbeProvider is a pre-refresh provider; its direct index assignment survives subsequent metadata merging and whole-item locking. New controls cover fresh/null indexes, omitted probes and audio under both replacement and lock choices. The real reprobe test resets the stale index independently for both replacement iterations and keeps its exact fresh-index, streams, runtime, timestamp, chapters and image assertions.

The first prepared overlay contained a test-only typed-reset error caught by independent review before application or execution. The corrected v2 uses a JSON numeric merge with conditional assignment; original preparation and actual failures remain preserved. The new index matrix is explicitly selected by both normal and fresh coverage filters.

Original reference and core records are preserved under `/tmp/ferrofin-m03-root-failure-evidence/`. Native source/culture evidence is in `/tmp/ferrofin-m03-native-culture-review/` and `/tmp/ferrofin-m03-native-culture-independent-review/`. The fixture-identity patch is in `/tmp/ferrofin-m03-probe-fixture-identity-followup/`, and the independently reviewed production index correction is in `/tmp/ferrofin-m03-probe-index-review/`. Durable follow-up planning is in `brain/knowledge/DASHBOARD_SETTINGS_S33_S34_AMBIENT_CULTURE.md`. Corrected native and core runs passed below; the common locale HTTP gate is separate evidence.

Validation passed:

- fmt: passed.
- core: 76 tests run: 76 passed, 2314 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 101,248/122,567 lines (82.61%). Fresh affected tests and the recorded prior profile were merged and exported against current binaries. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.

Evidence: `/tmp/ferrofin-dashboard-m03-checks.json`,
`/tmp/ferrofin-dashboard-m03-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 22.13 GiB generated target, 518.68 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| generation_disabled / elapsed_ms | 219.660 | 225.170 | 157.490 |
| interval_10 / elapsed_ms | 218.830 | 246.320 | 167.650 |
| interval_18 / elapsed_ms | 217.080 | 216.570 | 145.060 |
| interval_larger_than_runtime / elapsed_ms | 219.300 | 219.450 | 154.200 |
| disabled_again / elapsed_ms | 218.820 | 217.800 | 140.010 |
| negative_disables / elapsed_ms | 222.170 | 217.160 | 138.050 |
| single_original_replaced / elapsed_ms | 227.960 | 215.810 | 145.600 |
| multiple_original_preserved / elapsed_ms | 221.510 | 215.500 | 145.850 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.
