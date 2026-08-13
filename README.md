<div align="center">

# Ferrofin

**A drop-in Rust implementation of the Jellyfin media server.**

[![License: GPL-3.0-only](https://img.shields.io/badge/license-GPL--3.0--only-blue.svg)](LICENSE)
[![Rust edition 2024](https://img.shields.io/badge/rust-edition%202024-orange.svg)](rust-toolchain.toml)
[![API parity: 412/412 REAL](https://img.shields.io/badge/API%20parity-412%2F412%20REAL-brightgreen.svg)](docs/FEATURES.md)

<!-- CI + latest-release badges: add at public-repo publish, once the canonical host/registry is fixed. -->

</div>

Ferrofin speaks the **same HTTP API** as [Jellyfin](https://github.com/jellyfin/jellyfin), so
existing Jellyfin clients — the web UI, and native TV/mobile apps like Swiftfin, Findroid, and
Wolphin — connect to it **unchanged**. A client only ever sees an HTTP endpoint; it can't tell
the server is Rust. Point Ferrofin at an existing Jellyfin database and it adopts it in place.

> The name: a hermit crab moves into a shell it didn't grow. Ferrofin (*ferro*, iron/rust +
> *fin*, from Jellyfin) moves into Jellyfin's API shell — reusing the contract that clients
> depend on while replacing the body with idiomatic Rust.

- **Want to try it?** → [Quickstart](#quickstart)
- **Coming from Jellyfin?** → [Migrating from Jellyfin](#migrating-from-jellyfin)
- **What actually works?** → [Feature status](docs/FEATURES.md)
- **Want to contribute?** → [CONTRIBUTING.md](CONTRIBUTING.md)

<!-- Screenshot of jellyfin-web served by Ferrofin: add at publish (proof it drives real clients). -->

## Why

- **Small footprint, single static binary.** One `ferrofin-server` executable plus ffmpeg —
  no .NET runtime. Runs comfortably on hardware where the reference server strains.
- **Drop-in compatible.** Same API contract, same on-disk database format (pinned byte-equal
  to Jellyfin 10.11.8), same password hashes. Adopt an existing library with no re-scan; swap
  back to Jellyfin safely.
- **Honest about what it is.** Every operation in the Jellyfin API contract is wired to a real
  handler and cross-checked against a real Jellyfin server (see below) — not a demo with gaps
  papered over by fake `200`s.

## Benchmarks

> **Deferred.** Ferrofin is dramatically lighter on memory and faster to cold-start than the
> reference .NET server, but the "N× faster" throughput number swings widely run-to-run on
> identical code, so publishing it as a headline would be dishonest. A freshly-rerun,
> methodology-linked benchmark table lands here before the public release. The methodology
> already lives at [`suite/README.md`](suite/README.md) and
> [`suite/perf/README.md`](suite/perf/README.md).

The one reproducible compatibility stat, meanwhile: the parity harness diffs Ferrofin's
responses against a real `jellyfin/jellyfin:10.11.8` server across the whole API. **412/412
operations are real handlers, 201 of them deep-verified byte-for-byte, 0 untested.**

## Feature status

All 412 operations in the vendored contract are implemented — no `501` stubs. The
tiered matrix (verified · known-partial · not-implemented-by-design) is in
**[`docs/FEATURES.md`](docs/FEATURES.md)**. In short, working end-to-end: auth/users/QuickConnect,
library scan + live filesystem watch, browse/query, images, sessions/playstate/remote control,
WebSocket push, playlists/collections, direct play + live HLS transcode, Live TV (M3U/XMLTV +
DVR), SyncPlay, all 17 scheduled tasks, metrics/tracing, and backup/restore. Runtime plugin
installation is arriving as sandboxed WASM components (see
[Plugins](#plugins-compiled-in-extensions--sandboxed-wasm)); the deliberate gaps: .NET-style
native plugin loading (never — see below), DLNA SSDP discovery, and remote metadata
providers (feature-gated off by default).

## Quickstart

**Docker** (the release image bundles ffmpeg and the jellyfin-web client at `/web`):

```sh
docker run -d --name ferrofin \
  -p 8096:8096 \
  -v ferrofin-data:/data \
  <registry>/ferrofin:latest        # registry set at publish
```

**Helm:**

```sh
helm install ferrofin oci://<registry>/ferrofin/charts/ferrofin
```

See [`charts/ferrofin/values.example.yaml`](charts/ferrofin/values.example.yaml) for a worked
configuration.

**From source** (needs the pinned Rust toolchain; ffmpeg optional — its absence only disables
transcode):

```sh
cargo run -p ferrofin-server -- --data-dir ./data --bind 127.0.0.1 --port 8096
```

On a fresh database Ferrofin seeds an `admin` user and **logs a generated password — record
it** (or set `FERROFIN_ADMIN_PASSWORD` for a headless install). Then:

```sh
curl http://localhost:8096/System/Info/Public   # smoke test
# open http://localhost:8096/web, or point a native Jellyfin client at the server
```

Configuration is via CLI flags, `FERROFIN_*` environment variables, or `{data_dir}/config.toml`
(precedence in that order). The full surface is in [`docs/CONFIG.md`](docs/CONFIG.md).

## Migrating from Jellyfin

Ferrofin reads Jellyfin's database format directly. Point it at a data directory containing a
Jellyfin `jellyfin.db` (10.11.8) and on first boot it:

1. **Detects** the Jellyfin database and validates its migration set (refuses loudly rather
   than half-adopting an unexpected version),
2. **Backs it up** (`jellyfin.db.pre-ferrofin`, logged) before touching anything,
3. **Adopts it in place** — no re-scan, no re-import. Users, watch state, playlists, and Live
   TV config carry forward.

The adoption is **two-way**: Ferrofin only adds its own tables in a collision-proof namespace
and never rewrites Jellyfin's schema objects, so you can stop Ferrofin and start Jellyfin back
on the same database. Existing native Ferrofin databases upgrade in place across releases the
same way. The round-trip is covered by `suite/roundtrip.sh`.

## Plugins: compiled-in extensions + sandboxed WASM

Jellyfin loads plugins as .NET assemblies with **full trust**: any installed plugin runs with
the server's own privileges — it can read your filesystem, open network connections, and a
crash takes the server with it. Rust has no stable ABI, so Ferrofin *couldn't* copy that
model — but we also wouldn't want to. Instead, third-party functionality ships in two
deliberate forms:

**Compiled-in extensions** are Jellyfin plugins ported into the Ferrofin codebase itself,
reviewed like any other code and shipped inside the binary. They surface through the same
`/Plugins` API — dashboard entries, settings pages, runtime enable/disable. Intro Skipper,
File Transformation, and Merge Versions are ported today. This is the home for anything that
needs deep server internals. Details: **[`docs/EXTENSIONS.md`](docs/EXTENSIONS.md)**.

**WASM plugins** are how plugins Ferrofin's repo has never seen get
installed: drop a `.wasm` component into `{data_dir}/plugins/` and restart. **Security is
the reason for this design.** A WASM plugin runs in a sandbox with *no filesystem access, no
network access, and no view of server memory* — its entire world is the short, reviewable
list of capabilities Ferrofin explicitly exports to it (logging, its own settings,
host-mediated HTTP, read-only library queries, and writing its own media segments), plus
enforced memory and CPU-time limits (the memory limit is a per-plugin runaway ceiling, not
a reservation — a typical plugin's real footprint is a few MiB; see
[`docs/EXTENSIONS.md`](docs/EXTENSIONS.md)). A plugin that misbehaves is interrupted and disabled while the server keeps serving. A
plugin you install from a stranger's repo *cannot open your files or your network* — it
cannot read a byte of media content, browse the filesystem, or touch anything outside that
capability list. Be precise about what the list does grant, though: a plugin acting as a
metadata source can read your library's *catalog* (titles, ids, file paths) and can make
outbound HTTP requests that Ferrofin executes on its behalf (destinations logged, bodies
bounded, and private/LAN/loopback addresses refused unless you allowlist the plugin) — so
an actively malicious plugin could send your movie list somewhere public, and you should
still install plugins you have some reason to trust. The full does/does-not breakdown is
in [`docs/EXTENSIONS.md`](docs/EXTENSIONS.md). What the sandbox removes is the
catastrophic tail every full-trust plugin system carries: file access, raw sockets, and the
run-anything blast radius. One artifact runs on every platform and architecture, and
plugins can be written in any language that targets the WASM component model.

The trade-off is honest: extensions get full power under code review; WASM plugins get
safe installation without review. What Ferrofin will never do is load untrusted native code
into the server process.

## Architecture

A Rust workspace of ~20 crates, edition 2024, bottom-up dependency spine. The load-bearing
idea: clients depend on Jellyfin's **API surface**, not its code, so the contract is the
vendored OpenAPI spec and everything behind it is designed idiomatically in Rust (traits as
the dependency-injection seam; no ported OOP object hierarchy).

```
util ─┐
model ┼─ common ─┬─ db ──────┐
naming┘          │            │
keyframes        networking   ├─ traits ─┬─ mediaencoding ─ hls
chromaprint      health       │          ├─ drawing
                 metrics      │          ├─ providers
                              │          ├─ livetv
                              │          ├─ extensions
                              │          └─ wasm (plugin host)
                              └──────────────► core ─► api ─► server (bin)
```

Full crate map, C#→Rust mapping, and porting rules: **[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)**.

## Contributing

Ferrofin ports Jellyfin faithfully and gates every change behind `cargo fmt`, `clippy -D
warnings`, the full test suite, and an ≥80% per-crate coverage floor. See
[`CONTRIBUTING.md`](CONTRIBUTING.md) and, for the deeper operating guide, [`CLAUDE.md`](CLAUDE.md).

Security policy: [`SECURITY.md`](SECURITY.md). Changelog: [`CHANGELOG.md`](CHANGELOG.md).

## License

**GPL-3.0-only.** Ferrofin is a source-level derivative of Jellyfin. The Jellyfin repository is
internally inconsistent on this point (the root `LICENSE` is GPL-2.0, but the library `.csproj`
packages being ported declare `GPL-3.0-only`); Ferrofin follows the `GPL-3.0-only` metadata on
the specific crates it derives from. See [`LICENSE`](LICENSE).
