# Plan 5 — SQL boundary ratchet + known write-path N+1s

## Problem
Raw `sqlx::query` calls live in ~41 files (~335 call sites) across the workspace —
concentrated in `item_repository.rs` (36), `user_manager.rs` (29), `livetv/manager.rs`
(25), `library_scan.rs` (24), `item_persistence_service.rs` (24), `dto_service.rs`
(17), and a long tail. WHERE-clause logic (parent/ancestor, played/favorite,
item-values joins) is duplicated across 3–5 files. This is how agent-generated N+1s
keep appearing: each new manager grows its own SQL, copying whatever local pattern it
sees. A full DAL/ORM rewrite was considered and **rejected** (churn against a
196-endpoint parity-verified surface, and the schema is fixed by the Jellyfin-DB
drop-in requirement). The lazy alternative: enforce the boundary that already exists
(`hermit-db` entities + repository/persistence modules + `translate_query`) with a
ratchet, and fix the known write-path N+1 loops.

## Part A — the ratchet (a test, not a framework)
1. Add a workspace test (e.g. `crates/hermit-db/tests/sql_boundary.rs`, or a root
   `tests/` integration test if cleaner) that walks `crates/*/src` with `std::fs`,
   counts `sqlx::query` occurrences per file, and compares against a checked-in
   allowlist (`sql_boundary_allowlist.toml` or a const in the test): current files
   with their **current counts as ceilings**.
   - A file over its ceiling, or a new file with SQL not in the list → test fails
     with a message explaining the rule: *new SQL goes in a repository/persistence
     module in the allowlist's designated set; when you reduce a file's count, lower
     its ceiling in the same commit.*
   - Designated always-allowed modules (no ceiling): `hermit-db/src/**`,
     `hermit-core/src/item_repository.rs`, `translate_query.rs`,
     `people_repository.rs`, `media_stream_repository.rs`,
     `item_persistence_service.rs`, `item_count_service.rs`,
     `user_data_manager.rs`. (Adjust to actual repository-role files found at
     implementation time; the point is: SQL is allowed where querying *is* the job.)
2. This runs under `cargo nextest run --workspace` — no CI change needed, zero new
   dependencies, and it ratchets down over time as files get cleaned on touch.
3. Do **not** relocate existing SQL in this plan. Migration happens incrementally when
   files are touched for other reasons (the ceilings enforce monotonic improvement).

## Part B — fix the known write-path N+1 loops (concrete, bounded)
`crates/hermit-core/src/livetv/manager.rs`:
- ~lines 83–96: per-channel `INSERT` in a loop during M3U sync.
- ~line 145+: per-program `INSERT` in a loop during XMLTV guide sync (guides are
  thousands of programs — thousands of round-trips per refresh).

Fix both with chunked multi-row inserts: SQLite's default parameter limit is 999
(32766 on newer bundled versions — check the `rusqlite`/`libsqlite3-sys` build), so
chunk rows to stay safely under it (e.g. rows_per_chunk = 900 / columns_per_row),
inside one transaction per sync. `library_scan.rs` already does batch ingestion —
mirror its pattern. Live TV is fully implemented (phases 1–6 of the Live TV build are
done and wired); don't stub anything.

Also sweep for other per-row query loops in write paths (`grep -n "for .*{" -A3` near
`execute(` in hermit-core) and list any found in the report — fix only if equally
bounded, otherwise just report.

## Verification
- Standard gates: fmt, clippy `-D warnings`, `cargo nextest run --workspace`,
  coverage ≥80% on hermit-core.
- Part A: deliberately add a `sqlx::query` line to a non-allowlisted file, confirm the
  test fails with the explanatory message, remove it.
- Part B: the Live TV refresh scheduled task against a real M3U/XMLTV fixture (unit
  tests exist for the guide sync — extend them to assert row counts after a chunked
  insert; also time a 5k-program sync before/after and report).
- Parity untouched: Part B changes write batching only, not read shapes; run
  `benchmark/parity.sh` on the livetv endpoints anyway.

## Constraints
- Never create/switch branches; no AI-attribution trailers; tests in domain-named
  files; `///` docs on every new pub item.
- Do not use sqlx compile-time macros (`query!`/`query_as!`) — runtime queries only
  (project rule: no DATABASE_URL at build time).

## Conflicts
Part A touches no production code. Part B is confined to `livetv/manager.rs`. Safe in
parallel with Plans 1–4 (Plan 1/3 don't touch livetv).
