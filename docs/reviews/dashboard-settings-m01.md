# M01 — Default metadata language (`PreferredMetadataLanguage`)

The change carries the live server language/country fallback into named Person and BoxSet metadata and artwork consumers, while retaining item and library overrides. It also corrects the shared TMDB request contract: normalized preferred image language, `null`, and English unless already English; Movie/Series/Season/Episode detail requests append Images. Person requests append Images/ExternalIds/TvCredits/MovieCredits without `include_image_language`. Collection metadata supplies the fallback list; collection artwork requests all languages.

The oracle is explicit source pin `4910aafa1a`, principally `TmdbUtils.GetImageLanguagesParam`, `TmdbClientManager` and the Person/BoxSet providers. The real-router/native fixture checks actual stored metadata, downloaded Primary images, manual candidate order, overrides and regional `es-419` resolution. A changed Etag alone cannot satisfy readiness. Native Jellyfin 12.1.0 is separate supporting evidence, not a substitute for the pin; the actual results are recorded below.

Pinned Person/Collection caches omit country from their raw-language/ID key, so cold-ID phases require real requests while source-consistent warm omissions are allowed. Initial Person biography is observation only because PeopleValidator uses ValidationOnly; explicit FullRefresh establishes the consumer. S05/S06 provider execution/order and broader cache, identification and whitespace behavior remain independent work.

Reviewed entrypoints:

```sh
python3 /tmp/ferrofin-m01-independent-review/m01-checks.py --list
python3 /tmp/ferrofin-m01-independent-review/m01-coverage.py --list
```

Coverage must independently reach 80% for core and providers, with providers freshly measured in full. The locale fixture uses an isolated child-trusted offline TMDB mock and bounded owned-process cleanup. Evidence: `/tmp/ferrofin-m01-independent-review/review.md`; current manifest: `m01-validation-manifest.json` in that directory. 

## Failed runs and fixture corrections

The original native reference run timed out on the first French Person refresh. The stored biography was already French, but the refresh emitted no TMDB request. The pin caches Person and Collection metadata by ID and raw language for one hour; the initial scan had already warmed that entry. Existing central Primary images can also be protected as local images, and ProviderManager caches downloaded bytes by image URL for ten seconds. These are fixture prerequisites, not additional product defects established by this run.

The corrected M01 fixture uses the normal metadata editor to set a fresh provider ID for each language phase, confirms the stored ID, deletes only the isolated fixture item's existing Primary through the normal image route, and confirms its absence before refresh. Its mock artwork URLs include that provider ID to avoid the separate image-byte cache. It still requires a changed Etag, the exact saved biography/overview and provider ID, a new Primary tag, real selected-language image traffic, three correctly ranked manual candidates, and the source request parameters. M02's native fixture remains unchanged. No readiness or wire assertion was weakened.

The first normal validation passed core tests but failed providers test compilation because the new Movie Find regression lacked its function-local TmdbKind/TmdbSearchProvider imports. The correction adds only those imports. The original logs and check records are preserved under `/tmp/ferrofin-m01-root-failure-evidence/`; the failed locale fixture emitted no result JSON, which is explicitly recorded rather than treated as cleanup proof. Fixture preparation and source review are in `/tmp/ferrofin-m01-native-cache-review/`; the applied import patch and Q51 continuation proof are in `/tmp/ferrofin-m01-test-import-followup/`. All sixteen subsequent pieces still apply after that correction.

The next HTTP run reached the new movie-language assertion and failed because the list request did not ask for `Overview`. Both the pinned DtoService and Ferrofin project that field only when requested. The fixture now requests `fields=Overview`; its expected biography, name/rating readiness predicates and timeout remain unchanged. The failed HTTP log and the successful core/providers/reference checks are preserved as `movie-overview-field-*` in the same failure-evidence directory. This follow-up changes only that fixture request, so the completed crate and native-reference gates remain applicable; formatting, the corrected HTTP suite, build, native-after and strict lint are rerun. The exact added patch is recorded by the root-only proof adapter and then incorporated into the frozen continuation sequence.

The subsequent strict Clippy run rejected the offline HTTP mock's case-sensitive URL suffix check as a file-extension comparison. Replacing `ends_with(".png")` with `strip_suffix(".png").is_some()` preserves the exact accepted URL set and case sensitivity; no production or assertion changes are involved. The original lint failure is preserved as `mock-url-lint-*`. Formatting, HTTP and strict lint are rerun, with both exact fixture follow-ups included in the source provenance. Independent review is in `/tmp/ferrofin-m01-http-url-lint-independent-review/`.

Rust 1.98's subsequent strict lint also requires fixed-size array chunks in the TMDB detail-request regression. The test now uses `as_chunks::<4>().0.iter()` instead of `chunks_exact(4)`, with the same complete groups and unchanged wire assertions. The failed log/check record is retained as `request-chunks-lint-*`; formatting, all providers tests and strict lint are rerun after the correction. This affects test iteration only, and is recorded as the third fixture/test follow-up after the original Q51 stack.

Validation passed:

- fmt: passed.
- core: 76 tests run: 76 passed, 2308 skipped.
- providers: 859 tests run: 859 passed, 4 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 116,285/121,954 lines (95.35%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.
  Earlier attempt/seed diagnostics retained: warning: 22 functions have mismatched data.
- ferrofin-providers: 24,396/26,491 lines (92.09%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 6 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-m01-checks.json`,
`/tmp/ferrofin-dashboard-m01-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 21.14 GiB generated target, 519.10 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| fr/AR/Person / refresh_ms | 36.970 | 33.480 | 67.870 |
| fr/AR/BoxSet / refresh_ms | 38.000 | 35.430 | 57.470 |
| de/AR/Person / refresh_ms | 37.420 | 33.600 | 74.850 |
| de/AR/BoxSet / refresh_ms | 38.960 | 34.780 | 55.610 |
| es-419/AR/Person / refresh_ms | 36.470 | 33.470 | 60.370 |
| es-419/AR/BoxSet / refresh_ms | 40.880 | 33.410 | 19.530 |
| es-419/AR/Movie / refresh_ms | 211.260 | 201.460 | 62.340 |
| run / setup/configuration / timings.initial_scan_ms | 315.070 | 268.230 | 475.090 |
| run / setup/configuration / timings.config_read | 0.569 / 0.733 | 0.567 / 0.754 | 5.535 / 8.471 |
| run / setup/configuration / timings.ratings_read | 0.452 / 0.549 | 0.359 / 0.470 | 5.355 / 8.340 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.
