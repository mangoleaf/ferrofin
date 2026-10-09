# N02 — Save image paths in NFO (`SaveImagePathsInNfo`)

The change covers the actual enabled writer contract. Source pin `4910aafa1a`, `BaseNfoSaver.cs:131–142,842–857,928–989`, permits ImageUpdate saves when enabled and MetadataDownload otherwise. It emits the first Primary, all Backdrops and credited actor Primary thumbs; Logo/Thumb/Banner are excluded. Disabled metadata saves remove those fields. Original trimmed Backdrop paths are stably sorted under current culture before path substitution; remote paths remain unchanged and only emitted paths are mapped.

The culture seam covers immediate HTTP saves, provider-refresh batches, accepted idle scheduled-task entrypoints and detached N01 notifications. That does not establish culture parity for every nested full-library child worker. Request selection follows the pinned startup snapshot and query→cookie→Accept-Language precedence using the actual 105-resource supported catalog. Root executed separate .NET 10.0.12 comparer/request/raw-name/mixed-name oracles; the Rust implementations are independently exercised by the validation below. S28 generic NFO ordering, S29 provider priority queue, S30 dropdown catalog, S31 busy task draining, S33 ambient localized strings and S34 nested scan-worker culture remain open; N03's wider path-substitution contract is separate.

The native fixture independently checks false/true/false image fields, excluded kinds, actual actor import, a genuinely new Backdrop failure, Unicode ordering with reversing substitutions, queued/detached saves and a Swedish library scan after an owned restart with saved `UICulture=sv`. Persisted selectable `es_419` is tested through a real restart, reauthentication, Content-Language and an eligible changed-title NFO save. Before observations are bounded; after/reference assert the outputs. Jellyfin 12.1.0 remains separate supporting runtime evidence and the actual results are recorded below.

The Swedish scan prerequisite is explicit in the reviewed native fixture (`315f94d9`). Pin `Folder.cs:840–847` queues child work into `LimitedConcurrencyLibraryScheduler`; its persistent workers capture the first starter's context (`:132–137`) and later execute queued delegates without restoring the newest request's culture (`:150–189,225–244,300–312`). The fixture first reaps its English server, restarts the same isolated data with saved Swedish culture, reauthenticates the same administrator and verifies item/library identity plus unchanged NFO bytes before scanning. It then retains the fresh Completed-task and newly discovered Backdrop checks. This aligned worker lifetime isolates current-culture image ordering; it does not close S34's reused-worker/sequential behavior or S33's distinct `GetLocalizedString` context, and it does not broaden the completion claim for S29 or S31.

Reviewed entrypoints:

```sh
python3 /tmp/ferrofin-dashboard-n02-checks.py --list
python3 /tmp/ferrofin-dashboard-n02-coverage.py --list
```

Util/core/API/providers each require an independent 80% gate. Traits/server have compile/HTTP composition gates. The fixture override prevents existing FakeLocalization construction panics. Owned cleanup covers all three server lifetimes across the Swedish-worker and raw-culture restarts, preserves original errors, and keeps unique toggle phases. Evidence and linked current manifests: `/tmp/ferrofin-n01-n02-independent-review/`.

The original utility build failed because ICU Locale 2.3 has no `Default` implementation. The invariant empty-culture branch now uses the installed library's `Locale::UNKNOWN` (`und`) constant. ICU's root collator metadata uses that same undetermined locale without tailoring; public culture identity/name remain empty and the parent remains absent. The existing empty-culture comparer and all culture-name/ordering controls remain unchanged. The compile failure is preserved under `/tmp/ferrofin-n02-icu-root-locale-followup/`, and installed-API/source review under `/tmp/ferrofin-n02-icu-root-locale-independent-review/`. Earlier static review missed this constructor; it was the actual utility build that exposed it.

The corrected utility build exposed a real comparison failure: bundled ICU4X excludes search collation tables and silently falls back to ordinary ordering. German search therefore incorrectly placed ä before ae. A phonebook alias also fails the independent Arabic, Thai, symbol and Hangul controls. The isolated provider imports 22 actual search/searchjl tailorings (44 data/metadata files) from the official ICU78.1rc export matched to the installed ICU4X data. Safe trie constructors and an exact root-data comparison cover CE/context/Jamo/diacritic arrays plus 1,102,940 non-Hangul code points. Its initial expanded 500 comparisons pass, but the subsequent full matrix of 1,276 pairs, every 11,172 precomposed Hangul syllables and 29 culture identities exposes Korean, searchjl and parser/fallback differences. A first isolated vendor-condition experiment also failed, including ordinary-culture controls. These are preserved failed attempts; those earlier prototypes were not accepted into production or counted complete. The final implementation below includes source-data and vendor license notices, reproducible generation and a complete independent comparison. Evidence: `/tmp/ferrofin-n02-search-portable-data-review/`, `/tmp/ferrofin-n02-search-full-hangul-prototype-oracle-v2/` and `/tmp/ferrofin-n02-search-full-hangul-prototype-oracle-v3/`.

The subsequent isolated v4 consumer passes the strict complete comparison: 29 culture identities/acceptance/name/parent results, 1,276 pair signs for every valid culture, both comparison vectors for all 11,172 precomposed Hangul syllables, stable path ordering, 42 staged metadata/lazy-comparer cases and all 22 imported constructors. It restores the 67 modern Jamo entries omitted by genrb using private trie blocks, verified independently against all 24,510,464 logical source code points; remaining arrays and high/error values are unchanged. Korean archaic equality rules come from CLDR and apply only to the resolved ko/search tailoring; one reset expands through an imported root equality. A narrowly patched vendored ICU4X honors actual Jamo CE32s while retaining the original root shortcut. Primary conversion evidence is [ICU genrb](https://github.com/unicode-org/icu/blob/release-78.1rc/icu4c/source/tools/genrb/parse.cpp), [ICU collation builder](https://github.com/unicode-org/icu/blob/release-78.1rc/icu4c/source/i18n/collationdatabuilder.cpp) and [CLDR Korean collation](https://github.com/unicode-org/cldr/blob/release-48-2/common/collation/ko.xml). Actual prototype checks and strict comparison are preserved under `/tmp/ferrofin-n02-search-full-hangul-consumer-oracle-v4/`; comparison SHA256 is `f5caa57e794869ae2f4d15951e37ef69eb64d6142550f53bd89953417aad8b5b`. This establishes the prototype behavior; the actual production comparison is recorded next.

The integrated production implementation also passes the strict .NET comparison through the actual public Ferrofin API: 29 culture acceptance rows (25 usable comparers and four matching lazy failures), 31,900 pair signs, 558,600 syllable signs, stable sorting and all 42 staged construction/metadata/comparer cases. Every one of the 22 imported tailoring constructors succeeds. No copied parser or prototype adapter is used. The frozen production input manifest is `/tmp/ferrofin-n02-search-full-hangul-production-oracle-v4/oracle-manifest.json` (SHA256 `a8612fe20721b9930d283f9096c8bfa9938dcbc41a2078ef5b85ad74c64a91ad`); its actual strict comparison is `comparison.json` (SHA256 `6d58ea0524f0001df45cf79c677a65f22ea48746a14de7c0344291bf0e576214`). The normal utility gate runs all 15 culture tests, including the exact compiled-root compatibility, .NET pair oracle and every modern syllable. This source61 checkpoint preceded the integration and coverage corrections recorded below; final current-source results are listed separately.

A fresh run of the shipped data converter reproduces both generated files byte for byte (601,792 bytes total), with every pinned input hash unchanged. The actual regeneration record is `/tmp/ferrofin-dashboard-n02-data-regeneration-checks.json`; preserved input/output evidence is `/tmp/ferrofin-n02-data-regeneration-preserved/manifest.json`. All 195 utility tests pass with a fresh, unseeded 96.481217% line-coverage result (2,029/2,103 lines, 33 fresh profiles, no LLVM diagnostics).

The real server HTTP gate then exposed a separate actor-image lookup bug: enabled artwork emits the movie poster/fanart but omits the credited actor Primary image. The fixture imports an actual NFO credit, resolves its actual Person item and uploads that item's image. The saver incorrectly reads images using the credit row ID. Pinned `BaseNfoSaver.cs:963–972` instead calls `GetPerson(credit.Name)` and uses that exact Person's first Primary; `LibraryManager.GetPerson` derives the configured per-name path identity and requires the resulting row to be a Person. The strong off/on/off HTTP assertion is retained. The failed gate and source fixture are preserved under `/tmp/ferrofin-n02-http-actor-failure/`; the correction and its subsequent integration evidence are recorded below; the failed observation remains part of the review history.

The actor-image correction now derives aligned per-name Person IDs through the repository's already configured identity seam, retrieves those exact rows and accepts only Person items before reading their first Primary image. Director/Writer credits and a disabled image-path flag skip this lookup; duplicate identities reuse the image read. It does not fall back to credit IDs, arbitrary name matches or retained previous identities. No SQL statement or service-ownership wiring is added. The actual applied sequence is `56464ef3` plus the test-only owned-directory correction `d5722740`; the earlier run was interrupted during compilation before executing the fixed-root fixture. Its record is preserved under `/tmp/ferrofin-n02-actor-fixture-interruption/`. All 269 affected core tests, including the new real-repository identity regressions, pass. The subsequent actual HTTP run passes the unchanged actor off/on/off controls, then fails the distinct newly appended Backdrop path assertion. Its failed record, log and unchanged fixture are preserved under `/tmp/ferrofin-n02-http-new-backdrop-failure/`. The writer uses the logical slot count as its internal filename suffix, so a refresh-created suffix gap can make an append overwrite a currently referenced file. Pinned `ImageSaver.cs:121–128,528–583` separates the logical slot from its first-unused case-insensitive Backdrop filename. The additive `1aea87ad` fix retains the requested logical slot while allocating the first-unused positive filename suffix, then removes only a replaced file owned by that item after successful persistence. Case-insensitive duplicate references, external/shared files and paths escaping through a symlinked parent are retained. This repair covers internal upload allocation and owned replacement cleanup. The existing media-folder saver separately removes replaced local artwork. Complete eligible old-path cleanup and source deletion-error propagation need separate lifecycle review; the existing and new Ferrofin cleanup helpers log non-FileNotFound deletion errors where the pin propagates them. All 164 affected provider tests pass, including nine pinned filename cases and real file-preservation controls. The unchanged actual HTTP gate now passes, including the new path, count and image-read assertions after an NFO write error. Independent source review: `/tmp/ferrofin-n02-backdrop-independent-review/REVIEW-v3.md`.

This uses the existing default normalized by-name identity rules. It does not add support for the nondefault by-name identity contract (`EnableNormalizedItemByNameIds=false` with `EnableCaseSensitiveItemIds=true`) or the pin's 128-byte truncation/hash rule for overlong by-name folder names. Those are preexisting general identity gaps recorded separately as S35; the short and awkward-name controls establish the tested actor lookup without claiming full named-item lifecycle parity.

Strict Clippy exposed concrete parser/API style errors and intentional non-NFC oracle operands. The additive source65–70 corrections use explicit default types, consume the final owned base name, read the same last cookie by backward iteration, document the existing constructor panic, move an unchanged test helper before statements, use fixed test arrays and Rust’s standard no-op waker, and format explicit test defaults. A narrowly documented Unicode lint allowance preserves the exact generated .NET oracle module; all operand/sign payload bytes remain unchanged. No comparison assertion is relaxed. Failed Clippy attempts remain under `/tmp/ferrofin-n02-parser-lint-followup/`, `/tmp/ferrofin-n02-api-lint-followup/`, `/tmp/ferrofin-n02-api-test-order-lint-followup/` and `/tmp/ferrofin-n02-util-test-lint-followup/`. Final source70 passes formatting, all 15 culture tests and strict workspace Clippy for all targets/features. Historical source61 utility coverage is preserved under `/tmp/ferrofin-n02-source61-utility-coverage-preserved/` before its current-source refresh.

The final source70 public production API rerun also passes the strict comparison with unchanged inputs before and after. Its comparison SHA256 remains `6d58ea0524f0001df45cf79c677a65f22ea48746a14de7c0344291bf0e576214`; all 65 inputs and 45 production hashes are verified. The root-only runner adds `--locked` to the existing offline Rust command. Actual execution and saved strict-checker output are under `/tmp/ferrofin-n02-search-full-hangul-production-oracle-source70/`, with `actual-strict-receipt.json` recording the actual exit status. All 195 utility tests pass again without a seed: 2,031/2,104 lines (96.530418%), 15 fresh profiles and no LLVM diagnostics. Final normal gates pass 269 core, 122 API and 164 provider tests, unchanged real HTTP and native after observation, build, formatting, traits, SQL boundary and strict Clippy. Original source61 generation/utility evidence remains separately preserved. S35 general identity and S36 cleanup-error handling are still open planning work.

Every final changed-crate coverage gate passes with a fresh full suite and no seed: utility 195 tests, 96.530418%; core 2,406 tests, 95.393642%; API 914 tests, 86.384957%; providers 880 tests, 91.982437%. Core/API/provider seeded attempts measured 17.625382%/16.664945%/39.943918% and failed their threshold; their tests passed, and the runner retained both attempts before its required full fallback. Final utility/core exports have no LLVM diagnostics; API retains a 53-function mismatch warning and providers a six-function warning, including in their fresh full exports. No passing full run was repeated to suppress those diagnostics. Exact counts, profiles, tests, seed provenance and warnings remain in `/tmp/ferrofin-dashboard-n02-coverage.json`. After all coverage runtimes closed, generated `target/llvm-cov-target` was removed under the validation lock: target usage fell from 28.37 to 14.64 GiB with unchanged source status and all four verified profiles retained. Evidence: `/tmp/ferrofin-n02-post-coverage-target-cleanup.json`.

Validation passed:

- fmt: passed.
- util: 15 tests run: 15 passed, 180 skipped.
- core: 269 tests run: 269 passed, 2137 skipped.
- api: 122 tests run: 122 passed, 792 skipped.
- providers: 164 tests run: 164 passed, 716 skipped.
- traits: passed.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- data-regeneration: passed.
- build: passed.
- clippy: passed.
- production-culture-oracle: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-util: 2,031/2,104 lines (96.53%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.
- ferrofin-core: 118,042/123,742 lines (95.39%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.
- ferrofin-api: 41,806/48,395 lines (86.38%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 53 functions have mismatched data.
  Earlier attempt/seed diagnostics retained: warning: 53 functions have mismatched data.
- ferrofin-providers: 24,930/27,103 lines (91.98%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 6 functions have mismatched data.
  Earlier attempt/seed diagnostics retained: warning: 6 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-n02-checks.json`,
`/tmp/ferrofin-dashboard-n02-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 19.70 GiB generated target, 519.38 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Final 80-finding milestone validation:

- fmt: passed.
- workspace: 8332 tests run: 8332 passed (1 slow), 5 skipped.
- doctests: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s.

Evidence: `/tmp/ferrofin-dashboard-settings-80-milestone.json`. Workspace tests used the immutable
date-binding oracle; the opt-in compiled WASM guest tests remained disabled.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| images_False_Logo / edit_ms | 10.290 | 9.900 | 30.550 |
| images_False_Logo / image_update_ms | 10.410 | 11.950 | 46.360 |
| images_True_Backdrop / edit_ms | 11.190 | 13.220 | 15.640 |
| images_True_Backdrop / image_update_ms | 16.160 | 19.750 | 34.260 |
| images_False_Thumb / edit_ms | 11.320 | 11.930 | 15.480 |
| images_False_Thumb / image_update_ms | 10.410 | 18.560 | 27.630 |
| order_startup_default_after_ui_change / http_ms | 1.770 | 1.840 | 8.440 |
| order_startup_default_after_ui_change / observation_ms | 5026.990 | 253.800 | 84.420 |
| order_queued_swedish / http_ms | 2.350 | 2.040 | 8.120 |
| order_queued_swedish / consumer_ms | 2.480 | 203.540 | 58.790 |
| order_queued_swedish / observation_ms | 2.480 | 203.540 | 58.790 |
| order_image_query_precedes_cookie_header / http_ms | 12.860 | 21.300 | 29.010 |
| order_image_query_precedes_cookie_header / observation_ms | 5019.410 | 21.380 | 29.230 |
| order_detached_favorite_swedish / http_ms | 6.520 | 4.020 | 46.380 |
| order_detached_favorite_swedish / consumer_ms | 6.620 | 29.220 | 46.530 |
| order_detached_favorite_swedish / observation_ms | 6.620 | 29.220 | 46.530 |
| order_image_cookie_precedes_header / http_ms | 15.340 | 16.240 | 46.570 |
| order_image_cookie_precedes_header / observation_ms | 5038.480 | 16.310 | 46.780 |
| order_image_accept_language_swedish / http_ms | 14.710 | 16.570 | 31.500 |
| order_image_accept_language_swedish / consumer_ms | 14.830 | 16.630 | 31.690 |
| order_image_accept_language_swedish / observation_ms | 14.830 | 16.630 | 31.690 |
| order_image_reset_before_library / http_ms | 10.710 | 17.190 | 30.420 |
| order_image_reset_before_library / observation_ms | 5033.690 | 17.260 | 30.560 |
| swedish_scan_worker_restart / restart_ms | 2617.610 | 2598.930 | 4918.410 |
| order_full_library_background / http_ms | 0.910 | 0.660 | 11.810 |
| order_full_library_background / consumer_ms | 1.050 | 202.110 | 1072.070 |
| order_full_library_background / observation_ms | 1.050 | 202.110 | 1072.070 |
| order_images_disabled / edit_ms | 18.550 | 13.190 | 73.930 |
| image_save_survives_nfo_error / image_update_ms | 13.820 | 18.110 | 47.110 |
| order_reset_before_raw_restart / http_ms | 16.250 | 19.760 | 20.410 |
| order_reset_before_raw_restart / consumer_ms | 16.400 | 19.920 | 20.550 |
| order_reset_before_raw_restart / observation_ms | 16.400 | 19.920 | 20.550 |
| raw_saved_culture_restart / restart_ms | 506.400 | 506.110 | 4106.270 |
| raw_saved_culture_restart / http_ms | 15.300 | 24.130 | 353.990 |
| raw_saved_culture_restart / observation_ms | 5027.020 | 24.380 | 354.210 |
| order_startup_default_after_ui_change / consumer_ms | unavailable | 253.800 | 84.420 |
| order_image_query_precedes_cookie_header / consumer_ms | unavailable | 21.380 | 29.230 |
| order_image_cookie_precedes_header / consumer_ms | unavailable | 16.310 | 46.780 |
| order_image_reset_before_library / consumer_ms | unavailable | 17.260 | 30.560 |
| raw_saved_culture_restart / consumer_ms | unavailable | 24.380 | 354.210 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.
