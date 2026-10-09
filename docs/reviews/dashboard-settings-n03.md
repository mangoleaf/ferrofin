# N03 — NFO path substitution (`EnablePathSubstitution`)

The checkbox is persisted but unused by the pinned Jellyfin server as well as Ferrofin. This corrects the original finding's implication that Ferrofin alone omits a working source consumer. Web `1e507c588f353482a00333f84e36ddb7c8fc8221`, `src/apps/dashboard/routes/libraries/nfo.tsx:39,147–153`, exposes and saves it. Source pin `4910aafa1a` only references the property in `MediaBrowser.Model/Configuration/XbmcMetadataOptions.cs:12,21`.

Pinned `BaseNfoSaver.cs:980–988` unconditionally maps local artwork through `LibraryManager.GetPathAfterNetworkSubstitution`, while remote artwork retains its URL. The library manager applies the first successful server path mapping (`LibraryManager.cs:3505–3515`). This behavior is independent of the named NFO checkbox. Changing `SaveImagePathsInNfo` controls whether poster/fanart/actor thumbs are emitted; changing server mappings changes their exported paths. Neither operation moves the artwork itself. A newly invented checkbox consumer would diverge from this source contract.

The real HTTP regression independently saves false/true/false, checks each readback, forces a new titled NFO save and verifies mapped movie poster, backdrop and exact credited Person thumb each time. It verifies unchanged image catalogs and original files, then disables the image fields and removes the server mappings as independent controls. The isolated binary fixture repeats this through a generated video, a real NFO actor import and uploaded images; its owned-process verifier asserts exact emitted fields, saved values and all six phases. Jellyfin 12.1.0 is supporting runtime evidence, separate from the primary source pin. Before and after use the same production behavior because this finding adds verification only.

The only Rust changes are in the server integration test, which is exempt from line coverage under the contributor policy. No nonexempt library crate changes. Existing broad NFO ordering/path edge cases and additional findings remain separately tracked; this verification does not claim to repair the misleading checkbox in Jellyfin Web.

Validation passed:

- fmt: passed (2.23 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (22.99 s, including any compilation).
- build: passed (66.98 s, including any compilation).
- native-before: passed (6.17 s, including any compilation).
- native-reference: passed (8.98 s, including any compilation).
- native-after: passed (6.27 s, including any compilation).
- clippy: passed (1.67 s, including any compilation).
- doctests: 3 passed, 0 ignored (111.97 s, including any compilation).

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| path_substitution_0 / edit_ms | 15.048 | 14.043 | 15.971 |
| path_substitution_1 / edit_ms | 13.635 | 13.383 | 19.784 |
| path_substitution_2 / edit_ms | 13.332 | 14.521 | 12.606 |
| image_fields_disabled / edit_ms | 12.236 | 12.114 | 14.053 |
| mappings_removed / edit_ms | 13.255 | 11.921 | 13.370 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/n03/checks.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 416 GiB host reserve. Source and commits are preserved.


The independently reviewed HTTP title assertion was strengthened after the first native run. Source manifest 002 repeats formatting, HTTP (one test passed) and strict Clippy on the final test source; production code and the native fixture are unchanged from source manifest 001. Original tools, fixture and test source for manifest 001 are preserved alongside its evidence.
