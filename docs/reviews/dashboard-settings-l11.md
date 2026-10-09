# L11: automatic collections after library validation

`AutomaticallyAddToCollection` now drives a post-scan pass. Eligible movies in
enabled libraries are grouped by exact collection name, excluding virtual items
and alternate versions. At least two distinct movies are required to create a
collection; one can join an existing collection. Membership is additive, so
turning the option off preserves collections and previous membership. Ordinary
item refreshes do not run the global collection task.

NFO `<set>` names now reach the stored `CollectionName`, alongside the existing
TMDB mapping. The server shares its real collection manager through a weak
reference, avoiding a scanner/collection/library ownership cycle. The existing
scan-pass histogram gains one bounded `pass` value, `collections`.

Source: Jellyfin `4910aafa1a`, `CollectionPostScanTask` and `MovieNfoParser`.
Validation: 466 focused core tests pass, including NFO-to-scan collection
creation, library scope, exact names, alternate/virtual exclusions, repeated
passes, cancellation, and paging beyond 1,000 movies. All five native HTTP
phases match Jellyfin 12.1.0: off, on, existing single-member collection,
off with a new movie, and on again. Formatting, SQL boundary, server build,
and strict workspace Clippy pass.

Separate core coverage: **94.72%** (102,961 / 108,701 lines), combining the L10
baseline with 466 newly instrumented tests and exporting 22 current binaries.
LLVM function-data mismatch warnings persist; the four host file-watch-limit
failures in the full L07 core run remain an outstanding workspace gate.

Native parent/updated debug fixtures each run five complete scans. Initial
medians were **342 → 487 ms** (ranges **330–459 → 376–6,613**); the updated
outlier spent 6,253 ms in collections while compilation was active. A repeat
measured **458 → 353 ms** (ranges **345–1,043 → 335–440**), with reference parity
unchanged. These shared-host observations do not establish production latency
or large-library collection throughput.

Evidence: `/tmp/ferrofin-dashboard-l11-checks.json`,
`/tmp/ferrofin-dashboard-l11-coverage.json`,
`/tmp/ferrofin-dashboard-l11-native.json`,
`/tmp/ferrofin-dashboard-l11-{parent,updated}-repeat.log`, and
`target/dashboard-test-tmp/ferrofin-dashboard-l11-{ferrofin-4_xq6o7t,jellyfin-u_dn91in}/results.json`.
