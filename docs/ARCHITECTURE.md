# Architecture

The crate map, dependency spine, and C#→Rust mapping. Ferrofin reorganizes
Jellyfin's C# projects into ~20 Rust crates, following Rust idiom (traits as
the dependency-injection seam; merged implementation crates) rather than the
C# packaging split.

## Dependency spine (bottom → top)

```
util ─┐
model ┼─ common ─┬─ db ──────┐
naming┘          │            │
keyframes        networking   ├─ traits ─┬─ mediaencoding ─ hls
chromaprint      health       │          ├─ drawing
                 metrics      │          ├─ providers
                              │          ├─ livetv
                              │          └─ extensions
                              └──────────────► core ─► api ─► server (bin)
```

`ferrofin-traits` is the keystone: nothing above it imports `ferrofin-core`.
Jellyfin's C# has a reference cycle (implementations ↔ API) that is legal
under .NET runtime DI but **forbidden by Cargo**. Porting
`MediaBrowser.Controller`'s interfaces into a trait-only crate that both
handlers and implementations depend on breaks the cycle, and lets the server
run on stub implementations while a subsystem is unfinished.

Every manager trait is object-safe (`Arc<dyn Trait>`); `apps/ferrofin-server`
is the composition root that constructs the concrete implementations and
injects them into the API's `AppState`.

## Crate → C# project mapping

| Crate | Ports (C#) | Notes |
|---|---|---|
| `ferrofin-util` | Jellyfin.Extensions | leaf utils; string/path/natural-sort |
| `ferrofin-model` | MediaBrowser.Model | DTOs/enums (serde + utoipa); keeps the DLNA profile/StreamBuilder logic |
| `ferrofin-naming` | Emby.Naming | filename→media parsing; rich `[Theory]` test corpus transliterated |
| `ferrofin-keyframes` | Jellyfin.MediaEncoding.Keyframes | keyframe extraction structures |
| `ferrofin-common` | MediaBrowser.Common | config, app-paths, password hashing (PBKDF2-HMAC-SHA512/SHA1, byte-compatible with Jellyfin) |
| `ferrofin-networking` | Jellyfin.Networking | bind / published-URL resolution |
| `ferrofin-health` | (new) | lean liveness/readiness router |
| `ferrofin-metrics` | (new) | Prometheus `/metrics` with Jellyfin-parity names (see `docs/conventions/METRICS.md`) |
| `ferrofin-chromaprint` | (new) | audio fingerprinting (chromaprint) for the intro-skipper extension |
| `ferrofin-db` | Jellyfin.Database.* + Jellyfin.Data | **sqlx + SQLite**, runtime queries only; entities mirror the Jellyfin 10.11.8 schema byte-for-byte (see the schema-conformance test) |
| `ferrofin-traits` | MediaBrowser.Controller (interfaces) | `#[async_trait]` traits; the DI seam |
| `ferrofin-mediaencoding` | MediaBrowser.MediaEncoding | ffmpeg/ffprobe: probing, transcode arg-building (pure), the live transcode runtime behind a `Transcoder` seam trait |
| `ferrofin-hls` | Jellyfin.MediaEncoding.Hls | HLS playlist generation + stream manager |
| `ferrofin-drawing` | Jellyfin.Drawing + Emby.Photos | image resize/crop/format via the `image` crate |
| `ferrofin-providers` | MediaBrowser.Providers + Xbmc/LocalMetadata | local NFO always on; remote providers (TMDB/TVDB/OMDb/MusicBrainz/fanart) compiled in, gated by the per-library fetcher checkboxes |
| `ferrofin-livetv` | Jellyfin.LiveTv | M3U tuners + XMLTV guide, DB-backed DVR timers/recordings |
| `ferrofin-extensions` | (new — Tier 1a of the plugin design) | compiled-in extensions behind an `Extension` trait; see `docs/PLUGINS_UPSTREAM.md` |
| `ferrofin-wasm` | (new — Tier 1b of the plugin design) | sandboxed runtime-installed WASM plugin host (wasmtime + the `ferrofin:plugin` WIT world); see `docs/EXTENSIONS.md` |
| `ferrofin-core` | Emby.Server.Implementations + Jellyfin.Server.Implementations | the concrete manager implementations — the workhorse |
| `ferrofin-api` | Jellyfin.Api | axum router + handlers; depends only on `traits`+`model` |
| `ferrofin-server` (bin) | Jellyfin.Server | composition root: config → DB → ffmpeg discovery → wiring → serve |

## There is no domain-object hierarchy

Jellyfin's `BaseItem`/`Folder`/`Video` OOP tree is **not** ported — it is a
service layer in inheritance disguise. Trait signatures traffic in
`uuid::Uuid` (identity), `ferrofin-db` entities (persistence), and
`ferrofin-model` DTOs (presentation). Behavior that was a `virtual` method on
`BaseItem` becomes a free function over `BaseItemKind`
(`ferrofin-core/src/kinds.rs`).

## C# → Rust translation rules

- `class` (behavior) → `struct` + `impl`; `interface` → `trait`.
- `enum` → `enum`; `[Flags] enum` → `bitflags!` (critical: `TranscodeReason`
  is used with `|`).
- `Regex` strings copied **byte-for-byte**; `fancy-regex` when the pattern
  uses lookaround, else `regex`.
- `T?`/`Nullable<T>` → `Option<T>`; exceptions on the public path →
  `Result<T, <Crate>Error>` (per-crate `thiserror` enums; `anyhow` only in
  the binary).
- Namespace→module structure and method names (`snake_case`) preserved so
  C#↔Rust diffs cleanly.

## Persistence

`ferrofin-db` = sqlx + SQLite, runtime queries only (no compile-time
`query!` macros, so no `DATABASE_URL` at build time). The schema is pinned
**byte-equal to a real Jellyfin 10.11.8 database** — that is what makes the
drop-in adoption of an existing Jellyfin database possible (see the
`schema_conformance` test and `suite/roundtrip.sh`). Ferrofin-own additions
live in a collision-proof `Ferrofin*`/`FerrofinIX_*` namespace. Dynamic item
queries (Jellyfin's `ItemsController` surface) are built with sqlx
`QueryBuilder` in `ferrofin-core/src/translate_query.rs`.

## API contract

`contracts/jellyfin-openapi-<ver>.json` (vendored, pinned) is the
authoritative client contract. DTOs in `ferrofin-model` mirror it exactly
(PascalCase serde); routes are hand-written axum handlers matching the
spec's paths. The `contract_superset` test is a hard gate: the registered
route table must be a superset of the vendored spec (checked both
directions, plus a live probe that no contract route 404s). Unimplemented
routes return `501 Not Implemented`, never `404`.
