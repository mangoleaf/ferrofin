# N04 — extra thumbnail duplication (`EnableExtraThumbsDuplication`)

The production implementation already added for L17 consumes the named option live. This finding adds independent verification of that setting rather than another production change. Web pin `1e507c588f353482a00333f84e36ddb7c8fc8221` exposes it in NFO settings; primary server source is `4910aafa1a`.

Pinned `ImageSaver.cs:63–70,595–654` reads the flag and adds `extrathumbs/thumb<logical index>` for positive-index Compatible local backdrops. Index zero, mixed folders, Legacy convention and internal saving use one output. Turning the flag off stops subsequent copies and preserves existing duplicate files. Mixed-folder exclusion is source-backed and covered by existing policy tests; the new native fixture does not claim to exercise mixed folders.

The HTTP regression and independent native fixture check false/true/false, existing sentinel bytes, first-backdrop exclusion, Legacy and internal destinations, failure of a duplicate destination and recovery without restarting. The native verifier requires ten distinct phases, actual upload status, actual selected-file bytes and duplicate bytes, no thumb zero, and unchanged image catalog on failure. The pinned saver writes all outputs before publishing the image path (`ImageSaver.cs:156–175`), so a failed second copy propagates a server error rather than silently falling back. The first output can already have been written; this is not a transactional rollback claim. Broader cleanup failures remain separately tracked.

Only the server integration test changes, so no nonexempt library crate requires a coverage run. Existing source and worktree history are preserved. Before and after share the already implemented production behavior; Jellyfin 12.1.0 provides separate supporting runtime evidence.

The first reference attempt is retained as failed evidence: indexed POST uploads append in pinned `ImageController.cs:395–423`, whereas Ferrofin currently honors the index. The corrected native fixture uses the unindexed append route and clears cataloged backdrops before each phase and seeds a first backdrop when needed to obtain the requested logical index. It checks the actual starting count and retains the same substantive duplicate-byte and error assertions. A second retained attempt confirmed the already tracked S14 all-slots deletion behavior; clearing the entire catalog avoids relying on single-slot deletion. Indexed upload API parity remains a separate open finding; the setting implementation is independently verified through the shared append route.

The third retained reference attempt found an upstream corruption bug: on the two successful duplicate phases, Jellyfin writes the duplicate PNG but leaves the cataloged first output empty. Pinned `ImageController.cs:85–86` supplies a nonseekable `CryptoStream`; `ImageSaver.cs:137–145` copies it into a `MemoryStream` without rewinding, and `156–160` rewinds only for subsequent outputs. The final verifier asserts that exact empty first output for these reference phases and full PNG bytes for both Ferrofin outputs; every other successful reference output still requires full PNG bytes. Ferrofin intentionally retains correct artwork bytes rather than reproducing this source bug. Duplicate eligibility, live toggles and failure behavior remain independently gated.

Validation passed:

- fmt: passed (2.18 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (20.66 s, including any compilation).
- build: passed (0.44 s, including any compilation).
- native-before: passed (6.29 s, including any compilation).
- native-reference: passed (9.39 s, including any compilation).
- native-after: passed (6.29 s, including any compilation).
- clippy: passed (1.58 s, including any compilation).
- doctests: 3 passed, 0 ignored (6.18 s, including any compilation).

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| off_first / upload_ms | 21.216 | 17.896 | 62.241 |
| off_extra / upload_ms | 16.774 | 16.405 | 20.679 |
| off_preserves_existing / upload_ms | 17.551 | 20.555 | 17.060 |
| enabled_extra / upload_ms | 25.310 | 30.125 | 17.385 |
| disabled_again / upload_ms | 21.044 | 18.164 | 16.718 |
| enabled_first / upload_ms | 15.801 | 17.972 | 18.018 |
| legacy_extra / upload_ms | 13.885 | 17.424 | 14.427 |
| internal_extra / upload_ms | 9.706 | 8.812 | 15.465 |
| blocked_duplicate / upload_ms | 17.934 | 21.973 | 7.831 |
| repaired_duplicate / upload_ms | 16.063 | 19.588 | 17.518 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/n04/checks.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 416 GiB host reserve. Source and commits are preserved.

