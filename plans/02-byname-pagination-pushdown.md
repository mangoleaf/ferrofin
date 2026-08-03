# Plan 2 — Push pagination/name filters into the by-name aggregate query

## Problem
`item_values_with_counts` (`crates/hermit-core/src/item_repository.rs:153-252`) backs
`get_genres` / `get_music_genres` / `get_studios` / `get_artists` /
`get_album_artists` / `get_all_artists`. It **ignores** the caller's `start_index`,
`limit`, `search_term`, `name_starts_with`, `name_starts_with_or_greater`, and
`name_less_than` (all threaded in via `ByNameListQuery::base_query` in
`crates/hermit-api/src/handlers/by_name.rs:74-94` but silently dropped). It:

1. aggregates counts for **every** matching `ItemValues` row,
2. loads **every** by-name `BaseItems` row via an `IN` list,
3. sorts in memory (`items.sort_by(name)`),
4. returns the full set — which the handler then DTO-projects in full.

Cost scales linearly with library size. The benchmark fixture is only 2,637 items; the
release blocker is drop-in adoption of real Jellyfin libraries (10–100× bigger), where
this becomes seconds per request. It is also a likely **parity bug**: Jellyfin honors
`limit`/`startIndex`/name-range filters on these endpoints.

## Fix
Rework `item_values_with_counts` to do the work in SQL:

1. Add the name filters to the aggregate query's WHERE (against the by-name
   `BaseItems` row's `CleanName`/`SortName` — check which column Jellyfin filters on,
   in `ItemsByNameQuery` handling; use the parity harness as the oracle for
   case-sensitivity and range semantics).
2. Join the by-name `BaseItems` row **in the same query** instead of a second
   round-trip with an `IN` list (join `BaseItems` on `Id = iv."ItemValueId"` — the
   by-name row id *is* the ItemValueId per the current code).
3. `ORDER BY` the same key the current Rust sort uses (`Name`; verify against
   Jellyfin's ordering — the parity ledger already flagged sort divergences once, see
   the sort rules the harness checks), then `LIMIT ?3 OFFSET ?4` from
   `filter.limit`/`filter.start_index`.
4. When `filter.enable_total_record_count`, run a `COUNT(*)` variant of the same WHERE
   for `total_record_count`; otherwise skip it. Return a correct
   `QueryResult { start_index, total_record_count, items }` (today `from_items`
   fabricates them from the full set — keep the reported numbers identical for
   un-paged requests).
5. Keep the existing `ancestor_ids` EXISTS scoping and include/exclude content-type
   scoping exactly as-is.

Watch out: `search_term` on Jellyfin is a *contains* match while
`name_starts_with*`/`name_less_than` are prefix/range — don't merge their semantics.

## Verification
- Standard gates: fmt, clippy `-D warnings`, `cargo nextest run --workspace`, coverage
  ≥80% on hermit-core (`cargo llvm-cov nextest -p hermit-core --fail-under-lines 80
  --summary-only`).
- Unit tests: paged + name-filtered by-name queries against a seeded test DB (extend
  the existing item_repository test module; there is already a
  `paging_limits_and_reports_total` test to mirror).
- Parity (the real gate): `benchmark/parity.sh` diff vs Jellyfin for `/Studios`,
  `/Genres`, `/MusicGenres`, `/Artists`, `/Artists/AlbumArtists` with combinations of
  `limit`, `startIndex`, `nameStartsWith`, `searchTerm`. TotalRecordCount must match
  Jellyfin exactly, including when paged.
- Perf: `benchmark/run-phase-b.sh` with `PHASE_B_ENDPOINTS="studios persons"`; also
  note the unpaged case (benchmark passes no limit) still improves because the second
  query and the Rust sort disappear.

## Constraints
- Never create/switch branches; no AI-attribution trailers in commits; tests in
  domain-named files.
- Port faithfully from upstream C# (`Emby.Server.Implementations/Data/
  SqliteItemRepository.GetItemValues`) where semantics are ambiguous — C# is the
  oracle, except where the "don't port Jellyfin bugs" rule applies (document any
  accepted divergence).

## Conflicts
Self-contained to `item_repository.rs` + tests. Safe to run in parallel with Plans 1,
4, 5. Coordinate with Plan 3 only if it renames DTO entry points (it shouldn't).
