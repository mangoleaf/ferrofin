# hermit-api — Port Report (INTEGRATE)

Port target: `Jellyfin.Api` → `crates/hermit-api` (axum handlers over the
`hermit-traits` managers). Contract: vendored Jellyfin 10.11.8 OpenAPI spec
(`tests/data/jellyfin-openapi-10.11.8.json`).

Status: **All command gates return rc=0.** hermit-api clears the 80% line floor
*standalone* (**85.29%**), and the CI gate runs a **per-crate individual**
`-p <crate> --fail-under-lines 80` loop over `.port/gated-crates.txt` — no crate
hides behind a stronger sibling anymore. This run re-measured the REAL-vs-501
tally from source and found it has moved: **301 real / 111 still-501** (the
previous report's 269/143 is superseded — 32 more routes are now real).

## Gate results (this INTEGRATE run — 2026-07-23)

| Gate | Command | Result |
|------|---------|--------|
| Format | `cargo fmt --all` | clean (applied, no diff left) |
| Format check | `cargo fmt --all --check` | PASS (rc=0) |
| Build | `cargo build --workspace` | PASS — workspace compiles (rc=0) |
| Clippy | `cargo clippy --all-targets --all-features -- -D warnings` | PASS — rc=0, zero warnings |
| Tests | `cargo test --workspace` | PASS — rc=0, 0 failed (3 sequential full runs green; see flake note) |
| Coverage (per-crate loop, all 15 gated) | `-p <crate> --fail-under-lines 80 --summary-only` for each | PASS — rc=0 for every crate (lowest hermit-model 80.33%) |
| Coverage (this crate, standalone) | `cargo llvm-cov nextest -p hermit-api --fail-under-lines 80 --summary-only` | PASS — rc=0, **85.29% lines** (344 nextest tests, all pass) |
| Contract superset | `cargo test -p hermit-api --test contract_superset` | PASS — 4/4 |
| ffmpeg integration | `HERMIT_FFMPEG_TESTS=1` `-p hermit-mediaencoding --test segment_transcode_ffmpeg` + `-p hermit-hls --test hls_stream_manager_ffmpeg` | PASS — 8/8 against live ffmpeg |
| Newly-real live probe | boot `create_router(fake_state())`, probe formerly-501 routes | PASS — `/FallbackFont/Fonts`, `/Items/RemoteSearch/Movie`, `/Library/VirtualFolders`, `/Tmdb/ClientConfiguration` all answer **401** (real handler behind auth), not 501/404; `POST /SyncPlay/New` still **501** |

**Test flake note:** `hermit-keyframes::ff_probe::tests::get_keyframe_data_spawns_and_parses`
keys its temp dir on `std::process::id()` only. Three *sequential* `cargo test
--workspace` runs are green; it fails only when two full-workspace runs execute
*concurrently* against the shared target dir (a self-inflicted collision during
measurement). Mild test-isolation weakness (unique-suffix the temp dir), not a
product bug — no route or handler is affected.

### Per-crate coverage (individual `-p` runs, never merged)

| Crate | Lines | Crate | Lines |
|-------|-------|-------|-------|
| hermit-util | 94.49% | hermit-drawing | 91.32% |
| hermit-keyframes | 99.40% | hermit-providers | 91.23% |
| hermit-model | 80.33% | hermit-livetv | 89.74% |
| hermit-common | 86.84% | hermit-hls | 91.62% |
| hermit-naming | 96.49% | hermit-core | 83.89% |
| hermit-networking | 87.75% | hermit-api | 85.29% |
| hermit-health | 97.08% | hermit-mediaencoding | 83.60% |
| hermit-db | 100.00% | | |

All 15 clear the 80% line floor standalone (rc=0 each).

## Contract diff — CONFIRMED PASS

`tests/contract_superset.rs`, now **four** tests, all passing:

- `embedded_table_covers_the_whole_spec` — `VENDORED_ROUTES` equals the spec's
  `(method, path)` operations exactly (no drift, no extras).
- `registered_routes_are_a_superset_of_the_contract` — every vendored
  `(method, path)`, after axum-path normalization, is present in the router's
  registered table. **Not one contract route is dropped.**
- `probed_contract_routes_never_404` — building the real router and probing a
  spread of vendored paths never yields 404 (yields 501/401 — the route exists).
- `real_routes_have_no_duplicates` — **added this run.** Guards that
  `REAL_ROUTES` is a true set (no duplicate rows) and contains no route absent
  from the vendored contract, so the "REAL vs 501" count below stays honest.

## Route inventory — REAL vs 501 (measured, not estimated)

Counted by normalizing every `VENDORED_ROUTES` entry through
`routes::normalize_contract_path` and testing membership in
`handlers::REAL_ROUTES` (the exact set `create_router` mounts real handlers for).

| Metric | Count |
|--------|-------|
| Vendored contract **operations** (`VENDORED_ROUTES.len()`) | **412** |
| **REAL handlers** (`REAL_ROUTES.len()`, duplicate-free) | **301** |
| **Still 501** (on the shared `not_implemented` stub) | **111** |

Re-measured this run by a temp test that normalizes every `VENDORED_ROUTES`
entry through `routes::normalize_contract_path` and tests membership in
`handlers::REAL_ROUTES` (uppercasing the method, deduping both sides). Result:
`VENDORED_ROUTES.len()=412`, `REAL_ROUTES.len()=301` (no duplicates, confirmed by
`real_routes_have_no_duplicates`), `ROUTES_REAL=301`, `ROUTES_501=111`, and
`301 + 111 = 412`. **The previous report's 269/143 is superseded**: 32 routes the
last report listed as "core not-yet-wired" (RemoteSearch, VirtualFolders/library
admin, filesystem-monitor webhooks, subtitle transcode, FallbackFont) plus
`/Tmdb/ClientConfiguration` were wired to real handlers since that report — the
uncommitted working-tree changes to `handlers/mod.rs`, `library.rs`,
`item_lookup.rs`, `subtitles.rs`, `images.rs` are exactly those additions.

Any route not in the contract → **404** (`router::tests::unknown_route_returns_404`).

## Still-501 routes — the honest split (111 total)

Categorized from the exact still-501 list emitted this run (grouped by path
prefix, counts verified to sum to 111).

### Deferred-subsystem (77) — blocked on an un-ported subsystem recorded in `brain/DEFERRED.md`

| Un-ported subsystem | Count |
|---------------------|-------|
| Live-TV / channels (`/LiveTv/*` 41, `/Channels*` 5) — user choice, `DisabledLiveTvManager` | 46 |
| SyncPlay group-session (`/SyncPlay/*`) — user choice, no-op group manager | 22 |
| Plugin dynamic host (`/Plugins/*`) — **technical**, no Rust analogue to .NET assembly loading | 9 |

(DLNA/UPnP discovery contributes no distinct API paths — its profile/StreamBuilder
logic *is* ported into `hermit-model`; only the SSDP advertiser is deferred.)

### Remaining (34) → **30 plugin-provided + 4 core-not-yet-wired**

**Plugin-provided API surface (30)** — routes that exist only because a
third-party Jellyfin plugin (not the core server) registers them. Present as
`501` stubs purely so those clients don't 404; there is no core behaviour to
port:

- IntroSkipper / Intros / SkipButtonCss / `Episode/*/Timestamps` + IntroSkipperSegments (15)
- Package installer + repositories (`/Packages/*`, `/Repositories`) (6)
- Backup/restore plugin (`/Backup*`) (4)
- `/MediaSegmentsApi/*` (3)
- `/FileTransformation/*`, `/Jellyfin.Plugin.OpenSubtitles/*` (2)

(`/Tmdb/ClientConfiguration` is now **real**, so `/Tmdb/*` is no longer in this bucket.)

**Core, not-yet-wired (4)** — down from 35. The 31 formerly-listed routes
(RemoteSearch 11, VirtualFolders/library-admin 11, filesystem-monitor 5,
subtitle transcode 3, FallbackFont 2) are now **real** — verified by live probe
(they answer 401 behind auth, not 501). Only one group remains:

- Merge/split version variants (`/MergeVersions/{Merge,Split}{Episodes,Movies}`) (4) —
  honest reason: the merge/split *manager* seam isn't wired into the API
  composition root yet. `POST /Videos/MergeVersions` (the legacy path) *is* real;
  the four `/MergeVersions/*` operations stay on the shared `501` stub.

Note: **HLS/transcoding routes are no longer in the still-501 set.** The HLS
HTTP endpoints (`master/main/live.m3u8`, `hls1`/legacy segments,
`Videos/ActiveEncodings` DELETE, transcode `stream.{container}`, attachments)
are now REAL (`handlers::hls`, 100% lines). See `brain/DEFERRED.md`: the only
remaining transcode glue is the `StreamStatePlanner` seam wired at the Wave-8
composition root, which does not gate route reality (routes exist and serve).

## Coverage — now clean, gated per-crate

`cargo llvm-cov nextest -p hermit-api --fail-under-lines 80 --summary-only`:
**rc=0, TOTAL 85.29% lines** (regions 80.65%, functions 76.24%). 344 nextest
tests run, 344 passed, 0 skipped. The `--fail-under-lines` gate checks the
**lines** column (85.29%), which clears 80% on this crate alone. No coverage
carve-out exists in this crate. Every one of the 15 gated crates was run through
an **individual** `-p <crate> --fail-under-lines 80 --summary-only` (never a
merged multi-`-p` run) and each returned rc=0 — see the per-crate table above;
the lowest is hermit-model at 80.33%.

## Bottom line

- Every INTEGRATE **command** returns rc=0: fmt, fmt-check, build, clippy
  (`-D warnings`), workspace tests (3 sequential full runs green), the per-crate
  coverage gate for all 15 gated crates (each ≥ 80% lines standalone; hermit-api
  85.29%), the contract-superset hard gate (4/4), and the ffmpeg integration
  suite (8/8 against live ffmpeg).
- Route reality (re-measured this run): **301 of 412** vendored contract
  operations have real handlers; **111** remain on the shared `501` stub —
  **77 deferred subsystem** (LiveTV/channels 46, SyncPlay 22, Plugin host 9),
  **30 plugin-provided** (third-party plugin API surface, nothing to port),
  **4 core not-yet-wired** (`/MergeVersions/*` — merge/split manager seam not yet
  wired into the API composition root). The previous 269/143 is superseded.
- Live-router probe confirms the newly-real routes actually respond: booting
  `create_router(fake_state())` and hitting `/FallbackFont/Fonts`,
  `/Items/RemoteSearch/Movie`, `/Library/VirtualFolders`, `/Tmdb/ClientConfiguration`
  returns **401** (real handler behind auth), never 501/404; `POST /SyncPlay/New`
  still returns **501**.
- Contract superset holds: a real client never 404s on a contract path;
  unimplemented paths return 501, unknown paths return 404.
