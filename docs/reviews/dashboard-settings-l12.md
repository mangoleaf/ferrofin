# L12: metadata provider selection and order

The existing shared gates honor per-type enabled providers, including an empty
selection, and inherit server options only when the library has no per-type
entry. The scan folds enabled providers in configured/default/registration
order; Identify keeps the first result when providers share an external id.

Configured ranking now uses exact provider names, matching Jellyfin
`4910aafa1a`, `ProviderManager.GetConfiguredOrder` (`Array.IndexOf`). Enable
lists remain case-insensitive. Identify uses the same ranking helper as the
scanner and image selection instead of a separate case-insensitive lookup.

Validation: all **767 provider tests** pass (four environment-gated skips),
including selection, empty/absent inheritance, defaults, exact ranking and
Identify result precedence. Seven scanner metadata/artwork gate-and-order tests
pass. The real-server HTTP regression saves four live selections/orders and
verifies Identify: cleared selection, case-insensitive enabling, reversed
provider order, and an incorrectly cased order entry. Existing locale HTTP
checks also pass. Formatting, SQL boundary, build and strict workspace Clippy
pass. Separate provider coverage: **93.73%** (21,421 / 22,855 lines), from its
full instrumented suite and four current binaries.

This closes selection/ranking itself. Provider execution coverage is a separate
remaining gap: the path-less episode refresh implementation explicitly lacks
OMDb/TVDB metadata execution even though filesystem episode scanning supports
them. The living document records that as S05; this finding does not certify
those unfinished provider implementations.

Evidence: `/tmp/ferrofin-dashboard-l12-checks.json`,
`/tmp/ferrofin-dashboard-l12-coverage.json`,
`/tmp/ferrofin-dashboard-l12-scan-gates.log`, and
`apps/ferrofin-server/tests/dashboard_metadata_locale.rs`.

The subsequent full scanner regression also corrected two older plugin-order
fixtures to distinguish exact configured ranks from case-insensitive enabling.
All 468 focused scanner/persistence tests and strict workspace Clippy pass with
those expectations.
