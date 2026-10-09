# U23: block unrated content

Saved `BlockUnratedItems` now reaches browse, explicit-ID queries, Latest,
facets, counts, resumable folders and folder played-state queries. Preferences
are loaded once for each resolved query and shared by its page and total.
Previously the direct visibility resolver blocked an unrated movie while lists
and counts continued to expose it.

The query uses the inherited numeric rating: zero is a rating; an empty or
unrecognized classification is unrated. It respects the persisted `UnratedType`
on adopted rows and derives the class/source category for older Ferrofin rows
where that column is absent. Normal metadata writes and scans now stamp the
category. This needs no schema migration or destructive backfill.

User-policy persistence also preserves unnamed numeric enum values, rather than
writing Rust's debug representation. Reading and direct visibility accept the
same case-insensitive enum names and numeric values as upstream.

Source: Jellyfin `4910aafa1a`, `InternalItemsQuery.SetUser`,
`BaseItemRepository.TranslateQuery` and `ApplyParentalRestrictions`, plus the
`GetBlockUnratedType` overrides on media classes. Preserved source distinctions:

- Query construction excludes the canonical `Other` preference. Direct item
  checks retain it. Current Web presents seven specific categories, not `Other`.
- Plain folders and by-name items have direct-access exceptions; seasons leave
  direct blocking to their series/episodes. Query filtering uses the stored
  category, as upstream does.
- An explicit internal `HasParentalRating = true` selector skips unrated-kind
  filtering in `TranslateQuery`. Access-only predicates retain the user policy.
- Administrators do not receive an unrated-content exemption.

Validation: 390 focused core tests, 28 repository integration tests, eight
real-manager HTTP tests, SQL boundary, formatting, strict workspace Clippy and
server build pass. Regressions cover 15 class/source combinations on old rows,
score zero, explicit IDs, the query exception, folder played state, numeric enum
round-tripping and live saves. All 45 native HTTP observations pass for repeated
off/on/off changes across lists, Latest, genres, counts and direct access.

Native debug-build timings, 50 requests after ten warmups:

| Path | Before median / p95 ms | After median / p95 ms |
|---|---:|---:|
| Genres | 2.166 / 2.828 | 1.817 / 4.168 |
| Genre detail | 1.637 / 1.821 | 1.803 / 4.368 |
| Movie browse | 1.712 / 2.077 | 2.366 / 5.301 |

This is a three-movie fixture on a shared host with concurrent compilation,
not a production performance claim. The corrected browse returns one permitted
movie instead of all three and adds the preference lookup/filtering cost.

Evidence: `/tmp/ferrofin-dashboard-u23-checks.json` and
`/tmp/ferrofin-dashboard-u23-{before,after}-results.json`.
A clean coverage build passes all 2,138 core tests and reports **94.59% line
coverage**. LLVM still reports 18 mismatched-function warnings after cleaning;
the percentage passes the gate but retains that measurement limitation. Evidence:
`/tmp/ferrofin-dashboard-u23-coverage.json` and its referenced log.

U24's tag predicates remain a separate finding; loading their preferences does
not implement them.
