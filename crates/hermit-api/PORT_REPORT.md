# hermit-api — Port Report (INTEGRATE)

Port target: `Jellyfin.Api` → `crates/hermit-api` (axum handlers over the
`hermit-traits` managers). Contract: vendored Jellyfin 10.11.8 OpenAPI spec
(`tests/data/jellyfin-openapi-10.11.8.json`).

Status: **All command gates return rc=0.** The coverage caveat that dogged the
previous report is **resolved**: hermit-api now clears the 80% line floor
*standalone* (83.68%), and the CI gate runs a **per-crate individual**
`-p <crate> --fail-under-lines 80` loop over `.port/gated-crates.txt` — no crate
hides behind a stronger sibling anymore.

## Gate results (this INTEGRATE run — 2026-07-23)

| Gate | Command | Result |
|------|---------|--------|
| Format | `cargo fmt --all` | clean (applied, no diff left) |
| Format check | `cargo fmt --all --check` | PASS (rc=0) |
| Build | `cargo build --workspace` | PASS — workspace compiles |
| Clippy | `cargo clippy --all-targets --all-features -- -D warnings` | PASS — rc=0, zero warnings |
| Tests | `cargo test --workspace` | PASS — rc=0, 0 failed (all suites green) |
| Coverage (this crate, standalone) | `cargo llvm-cov nextest -p hermit-api --fail-under-lines 80 --summary-only` | PASS — rc=0, **83.68% lines** (290 nextest tests, all pass) |
| Contract superset | `cargo test -p hermit-api --test contract_superset` | PASS — 4/4 |
| ffmpeg integration | `HERMIT_FFMPEG_TESTS=1` `-p hermit-mediaencoding --test segment_transcode_ffmpeg` + `-p hermit-hls --test hls_stream_manager_ffmpeg` | PASS — 8/8 against live ffmpeg |

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
| **REAL handlers** (`REAL_ROUTES.len()`, now duplicate-free) | **269** |
| **Still 501** (on the shared `not_implemented` stub) | **143** |

Data-hygiene fix this run: `REAL_ROUTES` previously carried **two duplicate
rows** (`head /Genres/{genreName}/Images/{imageType}/{imageIndex}` and
`get /MusicGenres/{genreName}/Images/{imageType}/{imageIndex}`, already present
in the by-name image block, re-added by a stale "rows were omitted" comment).
The last report's **271 real / 176 still-501** was inflated by those dups and by
counting HLS as still-501. The corrected, self-consistent numbers are
**269 real / 143 still-501** (269 + 143 = 412).

Any route not in the contract → **404** (`router::tests::unknown_route_returns_404`).

## Still-501 routes — the honest split (143 total)

### Deferred-subsystem (77) — blocked on an un-ported subsystem recorded in `brain/DEFERRED.md`

| Un-ported subsystem | Count |
|---------------------|-------|
| Live-TV / channels (`/LiveTv/*`, `/Channels/*`) — user choice, `DisabledLiveTvManager` | 46 |
| SyncPlay group-session (`/SyncPlay/*`) — user choice, no-op group manager | 22 |
| Plugin dynamic host (`/Plugins/*`) — **technical**, no Rust analogue to .NET assembly loading | 9 |

(DLNA/UPnP discovery contributes no distinct API paths — its profile/StreamBuilder
logic *is* ported into `hermit-model`; only the SSDP advertiser is deferred.)

### Remaining (66) → **31 plugin-provided + 35 core-not-yet-wired**

**Plugin-provided API surface (31)** — routes that exist only because a
third-party Jellyfin plugin (not the core server) registers them. Present as
`501` stubs purely so those clients don't 404; there is no core behaviour to
port:

- IntroSkipper / Intros / SkipButtonCss / `Episode/*/Timestamps` + IntroSkipperSegments (14)
- Package installer + repositories (`/Packages/*`, `/Repositories`) (6)
- Backup/restore plugin (`/Backup*`) (4)
- `/MediaSegmentsApi/*` (3)
- `/FileTransformation/*`, `/Jellyfin.Plugin.OpenSubtitles/*`, `/Tmdb/*` (4)

**Core, not-yet-wired (35)** — the subsystem exists in the port; the handler
simply hasn't been mounted this push:

- Remote metadata search apply/typed-search (`/Items/RemoteSearch/*`) (10)
- Virtual-folder / library admin (`/Library/VirtualFolders*`, `/Library/PhysicalPaths`, `/Libraries/AvailableOptions`) (11)
- Filesystem-monitor change webhooks (`/Library/{Media,Movies,Series}/*`) (5)
- Merge/split version variants (`/MergeVersions/*`) (4)
- On-the-fly subtitle transcode (`.../Subtitles/.../Stream.*`, `subtitles.m3u8`) (3)
- Fallback font (`/FallbackFont/Fonts*`) (2)

Note: **HLS/transcoding routes are no longer in the still-501 set.** The HLS
HTTP endpoints (`master/main/live.m3u8`, `hls1`/legacy segments,
`Videos/ActiveEncodings` DELETE, transcode `stream.{container}`, attachments)
are now REAL (`handlers::hls`, 100% lines). See `brain/DEFERRED.md`: the only
remaining transcode glue is the `StreamStatePlanner` seam wired at the Wave-8
composition root, which does not gate route reality (routes exist and serve).

## Coverage — now clean, gated per-crate

`cargo llvm-cov nextest -p hermit-api --fail-under-lines 80 --summary-only`:
**rc=0, TOTAL 83.68% lines** (regions 79.20%, functions 74.24%). 290 nextest
tests run, 290 passed, 0 skipped. The `--fail-under-lines` gate checks the
**lines** column (83.68%), which clears 80% on this crate alone — the previous
report's "only passes because it's merged with hermit-core" caveat no longer
applies. No coverage carve-out exists in this crate.

## Bottom line

- Every INTEGRATE **command** returns rc=0: fmt, fmt-check, build, clippy
  (`-D warnings`), workspace tests, the per-crate coverage gate (83.68% lines
  standalone ≥ 80%), the contract-superset hard gate (4/4), and the ffmpeg
  integration suite (8/8).
- Route reality: **269 of 412** vendored contract operations have real
  handlers; **143** remain on the shared `501` stub — **77 deferred subsystem**
  (LiveTV/channels 46, SyncPlay 22, Plugin host 9), **31 plugin-provided**
  (third-party plugin API surface, nothing to port), **35 core not-yet-wired**.
- Contract superset holds: a real client never 404s on a contract path;
  unimplemented paths return 501, unknown paths return 404.
