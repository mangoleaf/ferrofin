# Architecture

The crate map, dependency spine, and C#→Rust mapping. Hermit reorganizes
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

`hermit-traits` is the keystone: nothing above it imports `hermit-core`.
Jellyfin's C# has a reference cycle (implementations ↔ API) that is legal
under .NET runtime DI but **forbidden by Cargo**. Porting
`MediaBrowser.Controller`'s interfaces into a trait-only crate that both
handlers and implementations depend on breaks the cycle, and lets the server
run on stub implementations while a subsystem is unfinished.

Every manager trait is object-safe (`Arc<dyn Trait>`); `apps/hermit-server`
is the composition root that constructs the concrete implementations and
injects them into the API's `AppState`.

## Crate → C# project mapping

| Crate | Ports (C#) | Notes |
|---|---|---|
| `hermit-util` | Jellyfin.Extensions | leaf utils; string/path/natural-sort |
| `hermit-model` | MediaBrowser.Model | DTOs/enums (serde + utoipa); keeps the DLNA profile/StreamBuilder logic |
| `hermit-naming` | Emby.Naming | filename→media parsing; rich `[Theory]` test corpus transliterated |
| `hermit-keyframes` | Jellyfin.MediaEncoding.Keyframes | keyframe extraction structures |
| `hermit-common` | MediaBrowser.Common | config, app-paths, password hashing (PBKDF2-HMAC-SHA512/SHA1, byte-compatible with Jellyfin) |
| `hermit-networking` | Jellyfin.Networking | bind / published-URL resolution |
| `hermit-health` | (new) | lean liveness/readiness router |
| `hermit-metrics` | (new) | Prometheus `/metrics` with Jellyfin-parity names (see `docs/conventions/METRICS.md`) |
| `hermit-chromaprint` | (new) | audio fingerprinting (chromaprint) for the intro-skipper extension |
| `hermit-db` | Jellyfin.Database.* + Jellyfin.Data | **sqlx + SQLite**, runtime queries only; entities mirror the Jellyfin 10.11.8 schema byte-for-byte (see the schema-conformance test) |
| `hermit-traits` | MediaBrowser.Controller (interfaces) | `#[async_trait]` traits; the DI seam |
| `hermit-mediaencoding` | MediaBrowser.MediaEncoding | ffmpeg/ffprobe: probing, transcode arg-building (pure), the live transcode runtime behind a `Transcoder` seam trait |
| `hermit-hls` | Jellyfin.MediaEncoding.Hls | HLS playlist generation + stream manager |
| `hermit-drawing` | Jellyfin.Drawing + Emby.Photos | image resize/crop/format via the `image` crate |
| `hermit-providers` | MediaBrowser.Providers + Xbmc/LocalMetadata | local NFO always on; remote providers (TMDB/TVDB/MusicBrainz/…) feature-gated |
| `hermit-livetv` | Jellyfin.LiveTv | M3U tuners + XMLTV guide, DB-backed DVR timers/recordings |
| `hermit-extensions` | (new — replaces the .NET plugin host) | compiled-in extensions behind an `Extension` trait; see `docs/PLUGINS_UPSTREAM.md` |
| `hermit-core` | Emby.Server.Implementations + Jellyfin.Server.Implementations | the concrete manager implementations — the workhorse |
| `hermit-api` | Jellyfin.Api | axum router + handlers; depends only on `traits`+`model` |
| `hermit-server` (bin) | Jellyfin.Server | composition root: config → DB → ffmpeg discovery → wiring → serve |

## There is no domain-object hierarchy

Jellyfin's `BaseItem`/`Folder`/`Video` OOP tree is **not** ported — it is a
service layer in inheritance disguise. Trait signatures traffic in
`uuid::Uuid` (identity), `hermit-db` entities (persistence), and
`hermit-model` DTOs (presentation). Behavior that was a `virtual` method on
`BaseItem` becomes a free function over `BaseItemKind`
(`hermit-core/src/kinds.rs`).

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

`hermit-db` = sqlx + SQLite, runtime queries only (no compile-time
`query!` macros, so no `DATABASE_URL` at build time). The schema is pinned
**byte-equal to a real Jellyfin 10.11.8 database** — that is what makes the
drop-in adoption of an existing Jellyfin database possible (see the
`schema_conformance` test and `suite/roundtrip.sh`). Hermit-own additions
live in a collision-proof `Hermit*`/`HermitIX_*` namespace. Dynamic item
queries (Jellyfin's `ItemsController` surface) are built with sqlx
`QueryBuilder` in `hermit-core/src/translate_query.rs`.

## API contract

`contracts/jellyfin-openapi-<ver>.json` (vendored, pinned) is the
authoritative client contract. DTOs in `hermit-model` mirror it exactly
(PascalCase serde); routes are hand-written axum handlers matching the
spec's paths. The `contract_superset` test is a hard gate: the registered
route table must be a superset of the vendored spec (checked both
directions, plus a live probe that no contract route 404s). Unimplemented
routes return `501 Not Implemented`, never `404`.
