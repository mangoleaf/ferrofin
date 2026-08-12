# Hermit

A from-scratch **Rust** port of [Jellyfin](https://github.com/jellyfin/jellyfin), the
open-source media server. Hermit speaks the **same HTTP API** as Jellyfin, so existing
Jellyfin TV clients (Wolphin, Swiftfin, Findroid) connect to it unchanged — the client
only ever sees an HTTP endpoint and can't tell the server is Rust.

The name: a hermit crab moves into an existing shell. Hermit moves into Jellyfin's API
shell — reusing its contract while replacing the body.

## Status

Ported component-by-component, strictly bottom-up, with a hard **80% line-coverage
gate** per crate. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the crate map
and the porting rules.

## License

**GPL-3.0-only.** Hermit is a source-level derivative of Jellyfin. The Jellyfin repo is
internally inconsistent (root `LICENSE` is GPL-2.0, but the library `.csproj` packages being
ported declare `GPL-3.0-only`); Hermit follows the `GPL-3.0-only` metadata on the specific
crates it derives from.

## Layout

```
crates/      library crates (hermit-util, hermit-model, hermit-naming, … hermit-api)
apps/        hermit-server (the binary / composition root)
contracts/   vendored Jellyfin OpenAPI spec — the authoritative client contract
docs/        architecture + conventions (metrics/tracing/logging) + plugin pins
.port/       the per-crate coverage-gate allowlist
```

## Build

```sh
cargo build --workspace
cargo test  --workspace
cargo llvm-cov nextest --fail-under-lines 80 -p <crate>   # coverage gate (line coverage; stable toolchain)
```
