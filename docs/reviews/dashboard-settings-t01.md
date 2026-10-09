# T01 — resolved encoder path display (`EncoderAppPathDisplay`)

FFmpeg-validated production startup now atomically saves the resolved FFmpeg executable into the named encoding configuration before services are composed. The existing encoding GET consequently returns the actual startup path to the dashboard's disabled display box. Updating this display preserves all typed encoding options and survives restart; invalid FFmpeg preserves a previously saved display. A real persistence failure propagates from startup.

Primary source `4910aafa1a`, `MediaBrowser.MediaEncoding/Encoder/MediaEncoder.cs`, `SetFFmpegPath`, validates startup selection and then saves `EncoderAppPathDisplay`. Web pin is `1e507c588f353482a00333f84e36ddb7c8fc8221`. The display is not an independent health endpoint or live executable selector. A client may still write the field with a full encoding document, and the next successful startup refreshes it, as in upstream. The hidden named `EncoderAppPath` startup selection discrepancy remains separate; this change does not implement that selector.

Production discovery carries the independently validated FFmpeg display path, including typed error context when a later ffprobe check fails. Invalid FFmpeg never attaches that context. The existing bare runtime fallback and capability behavior remain unchanged; fake compositions initialize the display path absent. Core regressions cover option preservation, restart and changed executables, absent or corrupt configuration, and a blocked persistence destination. The owned native fixture starts with two different copied FFmpeg/ffprobe executable paths, checks encoding readback and durability, separately exercises failed and missing FFprobe, and finally verifies that invalid FFmpeg preserves the previous display. It retains raw response bytes, status, encoding-file hashes and owned process exits. Before observations preserve the missing binding rather than asserting the proposed fix.

Only core needs separate line coverage; traits and server wiring have their compile/integration exemption. Relevant bootstrap, composition and HTTP checks still run. Supporting Jellyfin 12.1.0 observations remain distinct from the pinned source. No GPU availability is claimed.

The retained first reference attempt exposed a startup-lifecycle difference: the pinned host throws when FFmpeg validation fails (`ApplicationHost.cs:427–433`), and supporting Jellyfin exits with code zero after logging the fatal startup error. Ferrofin already permits startup with a fallback encoder. The corrected failure phase asserts the reference's actual exit, fatal log and unchanged persisted XML display, while Ferrofin must remain reachable and return its unchanged saved display. This finding corrects display persistence; it does not claim startup-failure lifecycle parity.

The second retained attempt caught a fixture alias: the save step changed the in-memory first GET observation while its captured response bytes remained correct. The fixture now copies the document before changing it; the independent verifier additionally requires every decoded JSON observation to equal its hashed raw response. No production assertion was relaxed.

Independent review found and corrected two issues in the preliminary implementation before coverage or commit. Pinned display persistence precedes FFprobe support checks; the final startup binding now preserves the validated FFmpeg display through missing and failing FFprobe validation without changing fallback runtime selection. Core startup also intentionally retries failed legacy XML import: the setter now leaves the named JSON absent while that import is pending, and the existing two-mode adoption regression repairs the XML, restarts, verifies imported options and then persists the display. The display remains unavailable until malformed legacy XML is repaired; a warning explains this preserved adoption limitation. Broader FFprobe/version/startup validation differences remain separate from the display setting.

The final native fixture adds separate missing and failing FFprobe phases. It retains the actual failing executable source and phase-scoped invocation JSON, requiring Ferrofin's real `-version` call and the reference's real `-only_first_vframe` support probe. Removing the executable must yield no new invocation receipt; both servers must persist the validated FFmpeg path. The original failed attempts remain intact. Strict Clippy rejected a floating-point equality in the adoption test; the final assertion compares exact IEEE bits for the exactly representable imported value rather than suppressing the lint or weakening the preservation check.

The normal targeted core, selected server, HTTP and binary build gates use source manifest 004. Manifest 005 changes only the core adoption test's floating-point assertion and the native fixture/verifier; production Rust is unchanged. The final full core coverage suite, expanded native runs, strict lint and doctests use manifest 005. Earlier successful gate evidence and failed attempts remain preserved.

Validation passed:

- fmt: passed (2.39 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (50.23 s, including any compilation).
- build: passed (76.99 s, including any compilation).
- native-before: passed (29.54 s, including any compilation).
- native-reference: passed (29.46 s, including any compilation).
- native-after: passed (17.21 s, including any compilation).
- clippy: passed (64.32 s, including any compilation).
- doctests: 3 passed, 0 ignored (64.10 s, including any compilation).
- ferrofin-core: 34 tests run: 34 passed, 2375 skipped (123.04 s, including any compilation).
- server: 48 tests run: 48 passed, 121 skipped (50.44 s, including any compilation).

Separate changed-crate line coverage:

- ferrofin-core: 118,160/123,865 lines (95.394179%), fresh full suite without seed.

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| resolved_startup_0 / startup.startup_ms | 9162.894 | 5588.300 | 7229.243 |
| resolved_startup_0 / response.milliseconds | 4.595 | 1.918 | 63.016 |
| save_preserves_display / save.milliseconds | 9.245 | 7.247 | 23.954 |
| save_preserves_display / response.milliseconds | 1.655 | 0.995 | 7.800 |
| resolved_startup_1 / startup.startup_ms | 5003.224 | 2767.516 | 5011.579 |
| resolved_startup_1 / response.milliseconds | 3.180 | 1.861 | 200.458 |
| validated_ffmpeg_failed_ffprobe / startup.startup_ms | 4667.577 | 2861.587 | 6345.355 |
| validated_ffmpeg_failed_ffprobe / response.milliseconds | 6.749 | 1.720 | 146.422 |
| validated_ffmpeg_missing_ffprobe / startup.startup_ms | 5363.224 | 2729.717 | 6277.692 |
| validated_ffmpeg_missing_ffprobe / response.milliseconds | 6.428 | 1.968 | 144.761 |
| failed_discovery_keeps_prior_display / startup.startup_ms | 4252.962 | 2501.774 | unavailable |
| failed_discovery_keeps_prior_display / response.milliseconds | 2.360 | 1.747 | unavailable |
| failed_discovery_keeps_prior_display / failure_ms | unavailable | unavailable | 2442.034 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t01/checks.json` and `/tmp/ferrofin-next10-root/evidence/t01/coverage.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 416 GiB host reserve. Source and commits are preserved.

