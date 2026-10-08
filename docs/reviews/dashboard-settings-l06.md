# L06: automatic series grouping uses invariant Unicode casing

Grouping already follows `EnableAutomaticSeriesGrouping` when series identities
are persisted. Its name fallback used Rust full lowercase, which differs from
.NET invariant lowercase: `ΟΣ` became `ος` instead of `οσ`. Two locations holding
`ΟΣ` and `οσ` consequently appeared as separate series even with grouping on.
The fallback now uses the existing invariant-casing helper. Provider IDs, item
IDs, language/library suffixes and disabled-grouping behavior retain their rules.

Source: Jellyfin `4910aafa1a`, `Series.CreatePresentationUniqueKey`,
`GetNameBasedGroupingKey` and `AddLibrariesToPresentationUniqueKey`.

All ten native observations match Jellyfin 12.1.0: five phases each for ordinary
and Greek names. Initial grouping joins both episodes; saving false leaves
persisted grouping until a scan splits it; saving true leaves it split until a
scan joins it. `SeriesCount` counts the two physical copies in both servers,
even when the browse list contains one group. Existing libraries need a scan
to recompute these persisted keys.

Validation: 274 focused normal core tests and 285 focused instrumented tests
pass, including invariant sigma and dotted-I cases. Formatting, SQL boundary,
strict workspace Clippy and server build pass. The separate current-binary core
coverage threshold is **94.84%** (101,952 / 107,496 lines), combining the full L05
profiles with this change's focused run; 29 LLVM function mismatch warnings
remain. L05's four host watcher failures remain an outstanding workspace gate.

Two alternating parent/updated native runs, each with three complete scans of
the same two-episode Unicode fixture, measured median **339.5 → 360 ms** (six
scans each; ranges **296–645 → 256–658 ms**). These are debug observations on a
shared host, not production performance numbers. The changed result is required:
the enabled phases now return one correctly merged series.

Evidence: `/tmp/ferrofin-dashboard-l06-checks.json`,
`/tmp/ferrofin-dashboard-l06-exact-coverage-summary.json`,
`/tmp/ferrofin-dashboard-l06-timings.json`,
`/tmp/ferrofin-dashboard-l06-unicode-ferrofin-tye6lpbq/results.json`, and
`/tmp/ferrofin-dashboard-l06-ferrofin-7vqiufnz/results.json`.
