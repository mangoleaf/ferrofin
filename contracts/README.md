# Contracts

The vendored **Jellyfin OpenAPI spec** — the HTTP contract Ferrofin implements and the
one real clients (jellyfin-web, Swiftfin, Findroid, Wolphin, …) are built against.

## `jellyfin-openapi-10.11.8.json`

- **What:** the spec a running Jellyfin **10.11.8** serves at `/api-docs/openapi.json`
  (OpenAPI 3.0.1, 337 paths, 412 operations). Jellyfin generates it at runtime with
  Swashbuckle, so it is captured from a live server rather than taken from the source tree.
- **Captured:** 2026-07-22.
- **Used by:**
  - `crates/ferrofin-api/tests/contract_superset.rs` — the hard gate: every path in the
    spec must be a registered route (an unported one answers `501`, never `404`), the
    registered table must not contain paths outside the spec, and a live probe checks
    nothing 404s.
  - `crates/ferrofin-api/src/contract_routes.rs` — generated from this file.
  - `apps/ferrofin-server` — embeds it and serves it at `/api-docs/openapi.json`.
  - DTO work in `ferrofin-model`: `jq '.components.schemas.<Type>' contracts/jellyfin-openapi-10.11.8.json`
    is the oracle for field names, casing and nullability.

## Why 10.11.8 and not 12.0

Two different pins, both deliberate. The **contract** is 10.11.8 because that is the API
the clients speak; 12.0-rc7's surface is smaller (364 operations — it drops the
`DynamicHls` family, `/MusicGenres`, `/CriticReviews` and more), so implementing it
instead would break clients. The **C# that behaviour is ported from** is `v12.0-rc7`
(`UPSTREAM_TAG` in `crates/ferrofin-api/src/handlers/mod.rs`) because it is the fixed,
corrected version of 10.11's logic. Where the two disagree on shape, the contract wins.

## Re-pinning

To target a different client version: capture its `/api-docs/openapi.json`, drop it here
under the versioned name, update the `include_str!` paths above and the pins in
`CLAUDE.md`, regenerate `contract_routes.rs`, and let `contract_superset` tell you what
moved.
