# Plan 3 — DTO projection pass: one path, fully batched

## Problem
`HermitDtoService` (`crates/hermit-core/src/dto_service.rs`, ~2950 lines) has **two
paths**:

- **Batch** (`get_base_item_dtos`, lines ~1386-1573): prefetches ~11 relations for the
  page (images, user data, media streams, provider IDs, people, person images, value
  IDs, chapters, trickplay, child counts, played counts), then loops `build_dto` with
  HashMap lookups. Good.
- **Single-item** (`get_base_item_dto` → `build_dto` with `prefetched=None`,
  ~line 1366): every relation falls back to its own query — up to ~24 queries per item,
  plus one `ItemValues` lookup **per genre/studio/artist name** (lines 596, 622, 646,
  661). Every handler that projects one item pays this, and agents keep copying the
  pattern into new handlers.

Remaining N+1s **inside the batch path**:

1. `set_item_by_name_info` is called **per item in the loop** (lines 1564-1566) when
   `ItemFields::ItemCounts` is requested. Each call is now one cheap GROUP BY (after
   the materialize+IN fix), but a 100-row Artists page is still 100 queries + 100
   `CleanName` lookups (`item_count_service.rs:156-162`).
2. `attach_artists` runs unconditionally (line ~1035), so artist/album-artist value-id
   pairs are resolved even when no artist field was requested (noted in the batch
   prefetch at ~1449-1482 — "attach_artists is unconditional").

Non-problem (verified, don't "fix"): `get_image_cache_tag` is a pure md5(path+ticks)
computation (`crates/hermit-drawing/src/processor.rs:460-481`) — no I/O. Leave the
per-image calls alone.

## Fix
1. **Single item = batch of one.** Reimplement `get_base_item_dto` as a call to the
   batch prefetch + `build_dto`, or have it build a `Prefetched` for `&[item]`. Then
   delete the `prefetched=None` fallback branches inside `build_dto` and its helpers
   (attach_people/studios/genres/artists per-name lookups) so the un-prefetched path
   **cannot be reached**. This is the durable fix: future handlers physically can't
   write the N+1.
2. **Batch `set_item_by_name_info`.** Add a batch method to `ItemCountService`
   (`hermit-traits` + `hermit-core/src/item_count_service.rs`): resolve all page items'
   `CleanName`s in one query, then one grouped count query
   (`GROUP BY iv."CleanValue", bi."Type"`) covering every name on the page. Wire it
   into `get_base_item_dtos` where the per-item loop currently calls
   `set_item_by_name_info` (dto_service.rs:1564-1566). Keep the per-item method
   delegating to the batch (single = batch of one). Trait must stay object-safe
   (no generics; `Arc<dyn Trait>` usable; keep the `_assert_object_safe` fn).
3. **Gate `attach_artists`** on the fields/kinds that actually need it. Determine the
   oracle first: Jellyfin emits `Artists`/`ArtistItems`/`AlbumArtists` on audio kinds
   regardless of `fields`, so the gate is likely *by item kind* (audio/music types)
   rather than by requested field — verify with the parity harness, then implement
   what Jellyfin does. Do not silently drop fields Jellyfin sends (strict Android TV
   SDK crashes on missing-where-Jellyfin-sends-non-null).
4. While in the file: `movies.rs:86-98` (hermit-api) calls `get_base_item_dtos` once
   per recommendation category (~8 calls/request → 8× prefetch). If cheap, collect all
   category items, project once, and reassemble per category preserving order/dupes.
   This endpoint (`movie_recommendations`) has a known concurrency cliff (~200 rps,
   serialization-bound) — this is likely a big part of it. Skip if it turns messy;
   note it in the report instead.

## Verification
- Standard gates: fmt, clippy `-D warnings`, `cargo nextest run --workspace`
  (+ doctests), coverage ≥80% per touched crate (hermit-core, hermit-traits is
  exempt).
- Parity: `benchmark/parity.sh` — single-item endpoints (`/Users/{id}/Items/{id}`,
  `/Persons/{name}`, `/Genres/{name}`) and list endpoints must stay byte-identical.
  The 196 deep-verified operations in the parity ledger are the regression net; run
  the harness before and after.
- Perf: `benchmark/run-phase-b.sh` with
  `PHASE_B_ENDPOINTS="item_detail persons items_mixed suggestions
  movie_recommendations"`. Expect item_detail (the single-item N+1) and persons
  (ItemCounts loop) to improve most.
- **Run the server and hit it over real HTTP** (project rule: green tests are
  necessary, not sufficient). Dev server needs `HERMIT_WEB_DIR` set or `/web` serves
  nothing.

## Constraints
- Never create/switch branches; no AI-attribution trailers; tests in domain-named
  files; every new `pub` item needs `///` docs (missing_docs is a warning and CI
  treats warnings as errors).
- No stubs/no-ops — deliver the full change or don't touch the sub-item (project
  "no deferring" rule).

## Conflicts
Touches the same `dto_service.rs` regions as Plan 1. **Run after Plan 1 lands.**
Step 2 also touches `item_count_service.rs` (same file Plan 1 reads but doesn't
modify). Plans 2/4/5 are independent.
