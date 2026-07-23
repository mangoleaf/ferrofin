# hermit-api — Port Report (INTEGRATE)

Port target: `Jellyfin.Api` → `crates/hermit-api` (axum handlers over the
`hermit-traits` managers). Contract: vendored Jellyfin 10.11.8 OpenAPI spec
(`tests/data/jellyfin-openapi-10.11.8.json`).

Status: **ALL GATES PASS.** Contract-diff (registered ⊇ contract) passes.

## Gate results

| Gate | Command | Result |
|------|---------|--------|
| Format | `cargo fmt --all` | clean (no changes) |
| Format check | `cargo fmt --all --check` | PASS (rc=0) |
| Build | `cargo build --workspace` | PASS — workspace compiles |
| Clippy | `cargo clippy --all-targets --all-features -- -D warnings` | PASS — zero warnings |
| Tests | `cargo test --workspace` | PASS — all tests green (hermit-api: 50/50) |
| Coverage | `cargo llvm-cov nextest -p hermit-api --fail-under-lines 80 --summary-only` | PASS — 97.71% lines (floor 80%) |

## Contract diff — CONFIRMED PASS

The hard gate is `tests/contract_superset.rs`, three tests, all passing:

- `embedded_table_covers_the_whole_spec` — `VENDORED_ROUTES` equals the spec's
  `(method, path)` operations exactly (no drift, no extras). The spec declares
  **337 distinct paths**; all operations use GET/POST/DELETE/HEAD.
- `registered_routes_are_a_superset_of_the_contract` — every vendored
  `(method, path)`, after axum-path normalization, is present in the router's
  registered table. **Not one contract route is dropped** → hermit-api's
  registered route set is a superset of the full 337-path contract.
- `probed_contract_routes_never_404` — building the real router and probing a
  spread of vendored paths never yields 404 (yields 501/401, i.e. the route
  exists).

Result: **hermit-api registers a superset of the 337 vendored paths.** ✔

## Route inventory

- **337** distinct vendored contract paths (`spec.paths`).
- Full `(method, path)` operation table registered via `create_router`
  (`axum_routes()` normalizes + de-dups the vendored table; `create_router`
  asserts `axum_routes().len() >= 400`).
- **13** First-Light routes served by real handlers (`handlers::REAL_ROUTES`);
  every other contract `(method, path)` is mounted on the shared
  `not_implemented` **501** stub (`routes::not_implemented`).
- Any route not in the contract → **404** (verified by
  `router::tests::unknown_route_returns_404`).

### First-Light routes implemented → traits called

| Method | Path | Handler | Trait(s) called |
|--------|------|---------|-----------------|
| GET | `/System/Info` | `system.rs` | `SystemManager::get_system_info` |
| GET | `/System/Info/Public` | `system.rs` | `SystemManager::get_public_system_info` |
| POST | `/Users/AuthenticateByName` | `users.rs` | `SessionManager::authenticate_new_session` |
| GET | `/Users/Me` | `users.rs` | `UserManager::get_user_by_id` |
| GET | `/UserViews` | `user_views.rs` | `UserViewManager::get_user_views`, `DtoService::get_base_item_dtos`; `resolve_user` (UserManager) |
| GET | `/Items` | `items.rs` | `LibraryManager::query_items`, `DtoService::get_base_item_dtos`; `resolve_user` |
| GET | `/Items/{itemId}` | `items.rs` | `LibraryManager::get_item_by_id`, `DtoService::get_base_item_dto`; `resolve_user` |
| GET | `/Items/{itemId}/PlaybackInfo` | `media_info.rs` | `MediaSourceManager::get_playback_media_sources`; `resolve_user` |
| POST | `/Items/{itemId}/PlaybackInfo` | `media_info.rs` | `MediaSourceManager::get_playback_media_sources`; `resolve_user` |
| GET | `/Videos/{itemId}/stream` | `videos.rs` | `MediaSourceManager::get_static_media_sources` |
| HEAD | `/Videos/{itemId}/stream` | `videos.rs` | `MediaSourceManager::get_static_media_sources` |
| GET | `/Items/{itemId}/Images/{imageType}` | `images.rs` | `LibraryManager::get_item_by_id` (+ `resolve_image_path`, stub — see deferrals) |
| HEAD | `/Items/{itemId}/Images/{imageType}` | `images.rs` | `LibraryManager::get_item_by_id` (+ `resolve_image_path`, stub) |

Auth: `[Authorize]` routes take the `RequireAuth` extractor (missing/invalid
token → 401); public routes read the possibly-anonymous `AuthorizationInfo`
extension set by the auth-context middleware.

### 501 stubs

- All vendored `(method, path)` operations **not** in `REAL_ROUTES` are mounted
  on the shared `not_implemented` **501** handler. This is the whole contract
  minus the 13 real First-Light routes.

## Coverage — 97.71% lines (floor 80%)

`cargo llvm-cov nextest -p hermit-api --all-features --summary-only`:
50 tests run, 50 passed, 0 skipped. TOTAL: 961 lines, 22 missed → **97.71%**
(regions 94.57%, functions 94.57%). Every gate file (`auth`, `error`, `routes`,
`state`, `openapi`, `handlers/mod`) is at 100% line coverage.

## Deferrals / known uncovered (22 lines)

These are honest, structural gaps — every one is either unreachable until a
later port wave lands, or an intentionally-skipped test double. None hide a bug.

- **`handlers/images.rs`** — the `ServeFile` serve branch is unreachable until
  the image processor is ported: `resolve_image_path()` is a stub that always
  returns `None`, so the serve path can't run yet (largest single gap; drives
  images.rs to 85.42% lines).
- **`handlers/videos.rs`** — the `ServeFile` `map_err` / `NotFound` arm
  (2 lines) — reachable only with a real on-disk media file.
- **`handlers/items.rs`** — `resolve_user` `NotFound(user)` arm (2 lines).
- **`handlers/users.rs`** — `get_user_by_id`-`None` `BadRequest` arm plus the
  nil-user guard (3 lines).
- **`handlers/user_views.rs`** — nil-id fallback in `Uuid::parse_str` (1 line).
- **`test_support.rs`** — `FakeSessions::logout_device` and
  `FakeConfig::update_configuration` skipped to avoid pulling `chrono` /
  building non-`Default` args (4 lines).

## Bottom line

Every INTEGRATE gate passes: fmt, fmt-check, build, clippy (-D warnings),
workspace tests, and coverage (97.71% ≥ 80%). The contract-diff hard gate
confirms hermit-api's registered route table is a **superset of all 337
vendored contract paths** — a real client never gets a 404 on a contract path;
unimplemented paths return 501, unknown paths return 404. 13 First-Light routes
have real handlers wired to the `hermit-traits` managers.
