# D04: external content in suggestions

Ordinary LiveTV-program similarity now uses the pinned source's public Program type alias. The storage lookup has no mapping for that alias, so the fallback returns no rows. It previously queried the stored LiveTvProgram kind and returned guide candidates. The movie-like and series-like Program branches keep their source-specific behavior.

The saved `EnableExternalContentInSuggestions` switch already updated all three movie candidate pools live: movie similarity, movie-like Program similarity and movie recommendations. With the setting enabled, those pools include Movie, Trailer and LiveTvProgram. Movie similarity excludes played candidates and applies selected-user access. The pinned query translator omits its IsMovie=true predicate when Movie or Trailer is included, so ordinary and series-like Program candidates can also qualify. Movie-like Program lookup uses a flat genre/tag query without the played predicate. Recent-played baselines remain Movies; liked baselines and people candidate pools expand. Configured remote similarity providers remain callable with either flag value.

Real-manager regressions use false → true → false on one manager and database, all three movie consumers, source-specific Program branches, a Trailer liked baseline, remote-provider execution and denied-library candidates with a grant-access positive control. The main matrix explicitly adopts distinct Program presentation keys to assert each candidate's eligibility. A separate regression keeps real guide NULL keys and verifies that Movie-containing user queries produce one keyless guide representative. Neither fixture changes guide key production.

Source contract: Jellyfin `4910aafa1a`. Native Jellyfin12.1 is separate runtime evidence. The native fixture requires a fresh Completed scan of real movie media/NFO and then adopts isolated Trailer/Program rows with explicit distinct keys. Those keys differ from the actual guide writer's NULL keys. The fixture verifies row consumers and does not establish guide/listings producer or plugin integration.

[S24 and S32](../../brain/knowledge/JELLYFIN_WEB_DASHBOARD_SETTINGS_REVIEW.md#additional-source-resolution-finding-from-implementation) remain separate view-production and home-video-resolution work. Earlier D04 verification-only preparation is superseded by this narrow default-branch correction.

Required validation includes affected core tests and coverage >=80%, SQL boundary, real HTTP, native before/after/reference, build, final-milestone doctests and strict workspace/all-targets/all-features Clippy. The final provenance below binds root's actual gates.

The initial verification-only run exposed two fixture errors: selecting the wrong database filename and expecting stored LiveTvProgram rows where the source query uses the unmapped Program alias. Correcting the native fixture demonstrated the production mismatch. The first corrected core matrix also used NULL presentation keys while expecting distinct rows; distinct adopted keys and a separate real-guide grouping regression now test both contracts. The original failing observations and corrections remain in `/tmp/ferrofin-d04-root-failure-evidence/` and the independent review records.

Target-only cleanup reduced generated build data from 22.07 to 17.89 GiB and preserved the worktree, commits and parent binary. Another build on this host reduced available space below the validation helper's original 512 GiB reserve, so the helper stopped before running tests. The reviewed reserve is now 416 GiB, above 11% of this filesystem's capacity; the 30 GiB target cap, serialized builds and all validation gates remain in force. `/tmp/ferrofin-dashboard-d04-target-cleanup.json` and `/tmp/ferrofin-d04-storage-guard-review/change.json` retain the measurements and exact helper change.

Final provenance is `/tmp/ferrofin-d04-source29-freeze/final-freeze.json` (SHA-256 `78fbc8bac963d228b232c3d86e5edce3337d721e09cc912884dffeecf8d442c4`). It binds all 33 source files to the root-tested tree, the source29 correction, 20 strictly applying unchanged successors, nine passing normal gates, and owned before/reference/after runs with no surviving children. The original checks record retains its earlier source28 manifest; the final snapshot supplies the separately reviewed correction without rewriting that history. Coverage is established by the actual run below.

The final eight-manifest routing proof is `/tmp/ferrofin-d04-source49-manifest-preparation/final-runner-list-checks.json` (SHA-256 `c1b7262bc0a175ab5b9cfbba92d95852772a26994156e0479f2c7e69e187b46f`): all 16 read-only checks/coverage list modes and 249 source/support artifact hashes passed. The independent NFO manifests use the same source49 queue. The D04 native roles select the source-correct fixture and owned-process wrapper for every phase.

Validation passed:

- fmt: passed.
- core: 37 tests run: 37 passed, 2337 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 116,042/121,705 lines (95.35%). Fresh affected tests and the recorded prior profile were merged and exported against current binaries. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.

Evidence: `/tmp/ferrofin-dashboard-d04-checks.json`,
`/tmp/ferrofin-dashboard-d04-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 19.22 GiB generated target, 507.08 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| False / movie / get_ms | 11.789 | 6.112 | 113.718 |
| False / movie-program / get_ms | 6.763 | 4.767 | 51.567 |
| False / series-program / get_ms | 6.083 | 4.285 | 65.649 |
| False / plain-program / get_ms | 8.482 | 4.282 | 12.718 |
| True / movie / get_ms | 12.976 | 5.463 | 62.304 |
| True / movie-program / get_ms | 7.492 | 5.384 | 42.780 |
| True / series-program / get_ms | 6.225 | 3.921 | 14.240 |
| True / plain-program / get_ms | 7.826 | 3.140 | 8.147 |
| False / movie / get_ms (occurrence 2) | 9.561 | 5.470 | 13.660 |
| False / movie-program / get_ms (occurrence 2) | 7.061 | 5.616 | 14.192 |
| False / series-program / get_ms (occurrence 2) | 4.506 | 5.102 | 17.059 |
| False / plain-program / get_ms (occurrence 2) | 4.595 | 3.683 | 24.602 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.
