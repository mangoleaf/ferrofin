# M04 — Chapter image resolution (`ChapterImageResolution`)

The change passes the live saved resolution through the actual chapter-image encoder request. Source pin `4910aafa1a`, `MediaEncoder.cs:651–676,747–760`, maps the eight fixed choices to 256×144, 426×240, 640×360, 854×480, 1280×720, 1920×1080, 2560×1440 and 3840×2160. MatchSource/unknown values omit scaling; audio requests receive no video-resolution argument.

The setting changes newly encoded images. It does not invalidate cached chapter images: pinned `ChapterManager.cs:145–225` keys reuse by date/position rather than resolution. The native case requires identical file hashes/dimensions after P144→P480 while cache remains, then removes image targets before testing newly encoded P480/MatchSource/P360/MatchSource output. It requires three nonempty real images at 0/26/35 seconds and checks actual pixels/dimensions, avoiding empty-result success. Recording-client and argument regressions cover all enum choices and audio exclusion.

Native Jellyfin 12.1.0 is separate runtime evidence; the actual results are recorded below. Existing encoder error/timeout/HDR/3D behavior remains outside this parameter repair. S26 chapter-name normalization is independently open and is not closed by correct image sizing.

Reviewed entrypoints:

```sh
python3 /tmp/ferrofin-m03-m04-independent-review/m04-checks.py --list
python3 /tmp/ferrofin-m03-m04-independent-review/m04-coverage.py --list
```

Canonical `/tmp/ferrofin-dashboard-m04-{checks,coverage}.py` aliases delegate to the reviewed adapter. Mediaencoding requires a fresh full suite and an independent 80% measurement; prior LLVM diagnostics are not carried as proof. The native replacement bounds owned cleanup and preserves original errors. Evidence: `/tmp/ferrofin-m03-m04-independent-review/EVIDENCE.md`. The common locale HTTP gate is separate from the resolution-specific native evidence below.

The first affected-test build failed because the new dashboard-resolution matrix called the private parent helper without importing it into the test module. A function-local `use super::image_resolution_parameter` fixes that test scope. Production code and every resolution, MatchSource, unknown-value, stream-selection and pixel assertion are unchanged. The original compile log and checks are preserved in `/tmp/ferrofin-m04-test-import-followup/`; independent review is in `/tmp/ferrofin-m04-test-import-independent-review/`.

Validation passed:

- fmt: passed.
- mediaencoding: 8 tests run: 8 passed, 913 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-mediaencoding: 17,122/18,821 lines (90.97%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 9 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-m04-checks.json`,
`/tmp/ferrofin-dashboard-m04-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 22.67 GiB generated target, 518.41 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| resolution_0_P144 / elapsed_ms | 138.900 | 134.780 | 196.780 |
| resolution_changed_cached_images_retained / elapsed_ms | 29.430 | 27.250 | 38.690 |
| resolution_1_P480 / elapsed_ms | 137.820 | 130.580 | 156.370 |
| resolution_2_MatchSource / elapsed_ms | 133.290 | 140.100 | 148.930 |
| resolution_3_P360 / elapsed_ms | 130.850 | 136.160 | 156.340 |
| resolution_4_MatchSource / elapsed_ms | 137.600 | 137.050 | 152.380 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.
