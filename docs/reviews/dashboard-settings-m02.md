# M02 — Default metadata country (`MetadataCountryCode`)

The existing live country inheritance remains the production path: item/library country takes precedence, and a blank library value falls back to the saved server country. This finding independently verifies the actual country consumers after M01's shared corrections: TMDB certification selection, live parental-rating tables, and regional language resolution for named Person/BoxSet metadata and artwork. `es-419` resolves to `es-AR` for Argentina and `es-MX` for the other tested country.

The oracle is source pin `4910aafa1a`, including `TmdbUtils.MakeParentalRating` and the language/country resolution passed to TMDB consumers. The real-router and native cases switch DE/FR/AR, clear library overrides and require the corresponding saved movie rating and country-specific `/Localization/ParentalRatings` output. Cold named-item IDs prove actual requests despite the pinned Person/Collection cache's omitted-country key. M02 adds verification rather than duplicating M01's production helper changes.

Native Jellyfin 12.1.0 remains a separate runtime comparison; the actual results are recorded below. S05/S06 provider execution/order and the inherited response-cache machinery remain open independently. The fixture does not infer cache parity from fresh requests.

Reviewed entrypoints:

```sh
python3 /tmp/ferrofin-m01-independent-review/m02-checks.py --list
python3 /tmp/ferrofin-m01-independent-review/m02-coverage.py --list
```

The changed nonexempt crate is providers, measured with a fresh full suite and an independent 80% gate. The same isolated offline TMDB fixture owns and bounds its server, media and TLS children; no host trust/cache/account is reused. Evidence: `/tmp/ferrofin-m01-independent-review/review.md`; current manifest: `m02-validation-manifest.json` in that directory.

Validation passed:

- fmt: passed.
- providers: 860 tests run: 860 passed, 4 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-providers: 24,504/26,599 lines (92.12%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 6 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-m02-checks.json`,
`/tmp/ferrofin-dashboard-m02-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 21.82 GiB generated target, 519.00 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| fr/DE/Movie / refresh_ms | 243.840 | 211.560 | 75.000 |
| fr/FR/Movie / refresh_ms | 243.990 | 207.790 | 67.000 |
| fr/AR/Movie / refresh_ms | 239.770 | 239.960 | 65.790 |
| es-419/AR/Person / refresh_ms | 33.830 | 36.120 | 22.760 |
| es-419/AR/BoxSet / refresh_ms | 34.420 | 39.660 | 29.220 |
| es-419/FR/Person / refresh_ms | 32.870 | 37.060 | 21.020 |
| es-419/FR/BoxSet / refresh_ms | 34.330 | 40.690 | 21.550 |
| run / setup/configuration / timings.initial_scan_ms | 320.200 | 313.570 | 518.380 |
| run / setup/configuration / timings.config_read | 0.504 / 0.690 | 0.469 / 0.678 | 6.486 / 8.576 |
| run / setup/configuration / timings.ratings_read | 0.429 / 0.534 | 0.397 / 0.549 | 5.618 / 8.352 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.
