# hermit-api — Port Report (INTEGRATE)

Port target: `Jellyfin.Api` → `crates/hermit-api` (axum handlers over the
`hermit-traits` managers). Contract: vendored Jellyfin 10.11.8 OpenAPI spec
(`tests/data/jellyfin-openapi-10.11.8.json`).

Status: **All command gates return rc=0.** One honest caveat on coverage (see
below): hermit-api *on its own* is 77.88% lines — it clears the 80% floor only
because the gate command scopes `-p hermit-api -p hermit-core` together and
llvm-cov applies `--fail-under-lines` to the **combined** TOTAL (82.19%).

## Gate results (this INTEGRATE run)

| Gate | Command | Result |
|------|---------|--------|
| Format | `cargo fmt --all` | clean (applied, no diff left) |
| Format check | `cargo fmt --all --check` | PASS (rc=0) |
| Build | `cargo build --workspace` | PASS — workspace compiles |
| Clippy | `cargo clippy --all-targets --all-features -- -D warnings` | PASS — rc=0, zero warnings |
| Tests | `cargo test --workspace` | PASS — rc=0, 2792 passed, 0 failed |
| Coverage | `cargo llvm-cov nextest -p hermit-api -p hermit-core --fail-under-lines 80 --summary-only` | PASS — rc=0, combined **82.19%** lines (489 nextest tests, all pass) |
| Contract superset | `cargo test -p hermit-api --test contract_superset` | PASS — 3/3 |

## Contract diff — CONFIRMED PASS

`tests/contract_superset.rs`, three tests, all passing:

- `embedded_table_covers_the_whole_spec` — `VENDORED_ROUTES` equals the spec's
  `(method, path)` operations exactly (no drift, no extras).
- `registered_routes_are_a_superset_of_the_contract` — every vendored
  `(method, path)`, after axum-path normalization, is present in the router's
  registered table. **Not one contract route is dropped.**
- `probed_contract_routes_never_404` — building the real router and probing a
  spread of vendored paths never yields 404 (yields 501/401 — the route exists).

## Route inventory — REAL vs 501 (measured, not estimated)

Counted by normalizing every `VENDORED_ROUTES` entry through
`routes::normalize_contract_path` and testing membership in
`handlers::REAL_ROUTES` (the exact set `create_router` mounts real handlers for).

| Metric | Count |
|--------|-------|
| Vendored contract **operations** (`VENDORED_ROUTES.len()`) | **412** |
| — across distinct **paths** | 325 |
| **REAL handlers** (operations with a wired handler) | **236** |
| **Still 501** (on the shared `not_implemented` stub) | **176** |

Note on the "337" figure: the previous report cited **337 distinct paths** and
**13** real routes. Both are stale. The current `VENDORED_ROUTES` table holds
**412 operations across 325 distinct paths**; `REAL_ROUTES` now covers **236**
of those operations. The 337 no longer matches the embedded table, and the
contract-superset test asserts against the embedded table + live spec JSON, not
against 337. Treat **412 / 236 / 176** as the source of truth.

Any route not in the contract → **404** (`router::tests::unknown_route_returns_404`).

## Still-501 routes grouped by reason (176 total)

### Deferred-subsystem (157) — blocked on an un-ported subsystem

| Reason (un-ported subsystem) | Count |
|------------------------------|-------|
| Live-TV / channels (`/LiveTv/*`, `/Channels/*`) | 46 |
| SyncPlay group-session (`/SyncPlay/*`) | 22 |
| HLS / transcoding pipeline (`hls`, `master/main/live.m3u8`, `Attachments`, `Videos/ActiveEncodings`) | 15 |
| Plugin/package installer host (`/Plugins/*`, `/Packages/*`, `/Repositories`) | 15 |
| IntroSkipper plugin (`/IntroSkipper`, `/Intros/*`, `/Episode/*/Timestamps`, `/SkipButtonCss/*`) | 15 |
| Virtual-folder / library admin (`/Library/VirtualFolders*`, `/Library/MediaFolders`, `/Library/PhysicalPaths`, `/Libraries/AvailableOptions`) | 11 |
| Remote metadata search/apply (`/Items/RemoteSearch/*`) | 10 |
| Third-party plugin routes (`/MediaSegmentsApi/*`, `/FileTransformation/*`, `/Jellyfin.Plugin.OpenSubtitles/*`, `/Tmdb/*`) | 6 |
| Filesystem-monitor change reports (`/Library/{Media,Movies,Series}/*`) | 5 |
| Backup/restore subsystem (`/Backup*`) | 4 |
| On-the-fly subtitle transcode (`.../Subtitles/.../Stream.*`, `subtitles.m3u8`) | 3 |
| Image-generation — splashscreen (`/Branding/Splashscreen`) | 3 |
| Encoding-options / fallback font (`/FallbackFont/*`) | 2 |

### Not-yet-done (19) — subsystem exists, handler simply not wired this push

| Reason | Count | Routes |
|--------|-------|--------|
| Image write/upload/delete | 6 | `POST/DELETE /Items/{itemId}/Images/{imageType}[/{imageIndex}]`, `POST .../Index`, `POST /UserImage` |
| Similar-items (recommendations) | 5 | `GET /{Items,Albums,Artists,Movies,Trailers}/{itemId}/Similar` |
| Merge/split version variants | 4 | `POST /MergeVersions/{Merge,Split}{Episodes,Movies}` |
| Scheduler cancel / trigger-config | 2 | `DELETE /ScheduledTasks/Running/{taskId}`, `POST /ScheduledTasks/{taskId}/Triggers` |
| Metadata editor descriptor | 1 | `GET /Items/{itemId}/MetadataEditor` |
| UserViews grouping options | 1 | `GET /UserViews/GroupingOptions` |

## Coverage — the honest breakdown

Gate command (`-p hermit-api -p hermit-core --fail-under-lines 80`): **rc=0**,
combined **TOTAL 82.19% lines** (regions 71.43%, functions 82.77%). 489 nextest
tests run, 489 passed, 0 skipped.

Per-crate, run in isolation:

| Crate | Lines | vs 80% floor |
|-------|-------|--------------|
| hermit-core | **84.14%** | above |
| hermit-api | **77.88%** | **below** |

So hermit-api by itself would *not* clear an 80% line floor. The gate as
specified passes because the two crates share one `--fail-under-lines` check
applied to the merged TOTAL, and hermit-core's coverage carries the average.
This is the single item that is "passing on the letter, not the spirit" — worth
raising hermit-api's own coverage before treating 80% as met for the API crate.

Lowest-covered hermit-api files (line %), the places to add tests:

- `handlers/lyrics.rs` — 44.00%
- `handlers/users.rs` — 45.27%
- `handlers/activity_log.rs` — 53.93%
- `handlers/config.rs` — 53.93%
- `test_support.rs` — 60.07% (test double; several arms intentionally unbuilt)
- `handlers/session.rs` — 65.85%
- `handlers/subtitles.rs` — 66.31%
- `handlers/playstate.rs` — 69.65%
- `handlers/item_update.rs` — 70.30%
- `handlers/library.rs` — 70.41%
- `handlers/playlists.rs` — 72.07%
- `handlers/remote_images.rs` — 72.73%

Several low numbers are error/not-found arms and deferred serve branches
(e.g. image-serve is unreachable until the image processor is ported), but the
list above is real, addressable API-crate coverage debt, not all structural.

## Bottom line

- Every INTEGRATE **command** returns rc=0: fmt, fmt-check, build, clippy
  (`-D warnings`), workspace tests (2792 pass), the coverage gate (82.19%
  combined ≥ 80%), and the contract-superset hard gate (3/3).
- Route reality: **236 of 412** vendored contract operations now have real
  handlers; **176** remain on the shared `501` stub — **157 blocked on a
  deferred subsystem**, **19 simply not-yet-wired** this push.
- Contract superset holds: a real client never 404s on a contract path;
  unimplemented paths return 501, unknown paths return 404.
- One caveat, stated plainly: **hermit-api alone is 77.88% lines** and only
  clears the 80% floor because the gate averages it with hermit-core.
