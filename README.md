<div align="center">

# Ferrofin

**A drop-in Rust implementation of the Jellyfin media server.**

[![License: GPL-3.0-only](https://img.shields.io/badge/license-GPL--3.0--only-blue.svg)](LICENSE)
[![Rust edition 2024](https://img.shields.io/badge/rust-edition%202024-orange.svg)](rust-toolchain.toml)
[![API surface: 412/412](https://img.shields.io/badge/API%20surface-412%2F412%20REAL-brightgreen.svg)](docs/FEATURES.md)

[![CI](https://github.com/mangoleaf/ferrofin/actions/workflows/ci.yml/badge.svg)](https://github.com/mangoleaf/ferrofin/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/mangoleaf/ferrofin)](https://github.com/mangoleaf/ferrofin/releases)

</div>

Ferrofin speaks the **same HTTP API** as [Jellyfin](https://github.com/jellyfin/jellyfin), so
existing Jellyfin clients (the web UI and native TV/mobile apps such as Swiftfin, Findroid, and
Wolphin) connect to it **unchanged**. A client only ever sees an HTTP endpoint; it cannot tell
the server is Rust. Point Ferrofin at an existing Jellyfin database and it adopts it in place.

- **Want to try it?** → [Quickstart](#quickstart)
- **Coming from Jellyfin?** → [Migrating from Jellyfin](#migrating-from-jellyfin) (read the
  one-way warning first)
- **What actually works?** → [Feature status](docs/FEATURES.md)
- **Want to contribute?** → [CONTRIBUTING.md](CONTRIBUTING.md)

## Why Ferrofin

- **Faster, and a fraction of the memory.** On the same machine and library, Ferrofin
  renders the home screen **3.4× faster** than Jellyfin 12.0-rc7, cold-starts **15.8×
  faster**, and sits at **61 MiB idle against 409 MiB** (6.7× lighter), with peak memory
  under load 3.8× lower. Numbers, method and caveats are in [Benchmarks](#benchmarks).
- **One binary, no runtime.** A single `ferrofin-server` executable plus ffmpeg. No .NET
  runtime, no JIT warm-up. The Docker image bundles both jellyfin-ffmpeg and the jellyfin-web
  client, so `docker run` gives you the whole server, web UI included.
- **Drop-in compatible.** Same API contract, same on-disk database format (pinned to
  Jellyfin 10.11.8), same password hashes. Adopt an existing library with no re-scan.
- **Plugins cannot own your server.** Jellyfin loads plugins as full-trust .NET code inside
  the server process. Ferrofin does not, and will not. Third-party plugins run as
  sandboxed WASM with no filesystem or network access of their own. This is the one place
  Ferrofin deliberately diverges from Jellyfin; see [Plugins](#plugins-the-one-deliberate-divergence).
- **Every route ported** All 412 operations in the Jellyfin API contract are wired to
  working handlers, developed by cross-checking responses against a real Jellyfin server.
  No `501` stubs, no fake `200`s.

## About this project

Ferrofin began as a personal experiment in agentic engineering: a large Rust project taken from start to
release with AI agents writing most of the code and me, a software engineer of 15+ years,
steering the architecture, decisions, reviewing, and testing. I expect the first thing people will call
this is an "AI slop fork", and I understand the skepticism. I had a working server after two
days and could have released it then. Instead I spent the next six weeks using it as my home
media server, testing features, fixing performance issues and bugs, reshaping the architecture, adding tests and
gates, benchmarking, and closing the parity gap against Jellyfin. I fully intend to continue
using Ferrofin personally and to improve it over time.

I will not claim 100% parity with Jellyfin or that Ferrofin is bug free. At ~360k lines of
Rust there was more code than I could read, and the agents made questionable decisions at
times without asking. The performance gains over Jellyfin have been verified and the benchmarks back them
up, but there is still room for improvement.

Up until this first public release I have worked on Ferrofin solo. Most of my attention was on my 
personal definition of the golden path. Areas that could use more attention include Live TV and the large number of settings.
If something does not behave the way Jellyfin does, opening an issue describing the current and expected behaviour would be appreciated.
PRs or plugin development are welcome too if anyone wants to get involved!

## Benchmarks

<!-- BEGIN GENERATED BENCHMARKS — do not edit by hand. Regenerate with:
     python3 bench/report.py --readme README.md bench/runs/v1.0.0 bench/runs/v1.0.0-run2 bench/runs/v1.0.0-run3 -->
Ferrofin `v1.0.0` against **Jellyfin 12.0-rc7** and **Jellyfin 10.11.8**, measured 2026-09-04 on one machine (AMD Ryzen 9 9950X3D), each server alone in a container pinned to 8 dedicated cores with an 8 GiB limit and no swap, over the same library of 3,001 movies, 250 series and 7,490 episodes. Every figure is the **median of 3 full runs**. No request failed on any server at any load level.

The screen rows are what a client actually does: each is the exact request set jellyfin-web issues for that screen, replayed at 5 screens per second (the "loaded" level). Latency reads **p50 / p95 / p99 in milliseconds**; the last column compares p50 with Jellyfin 12.0-rc7.

| | Jellyfin 12.0-rc7 | Jellyfin 10.11.8 | **Ferrofin** | Ferrofin vs 12.0-rc7 |
|---|---|---|---|---|
| **home** screen | 44 / 51 / 57 | 64 / 73 / 77 | **13 / 16 / 19** | **3.4× faster** |
| **movies** screen | 38 / 43 / 47 | 24 / 28 / 31 | **25 / 30 / 32** | **1.5× faster** |
| **detail** screen | 98 / 161 / 165 | 20 / 29 / 33 | **9 / 12 / 13** | **10.9× faster** ⚠[1] |
| **series** screen | 97 / 101 / 224 | 18 / 21 / 27 | **8 / 10 / 11** | **12.1× faster** |
| **search** screen | 70 / 234 / 255 | 98 / 121 / 184 | **26 / 36 / 42** | **2.7× faster** |
| **playback** screen | 31 / 37 / 55 | 5 / 20 / 33 | **10 / 13 / 45** | **3.1× faster** ⚠[2] |
| cold start (restart → home screen) | 2746 ms | 2162 ms | **174 ms** | **15.8× faster** |
| direct-play TTFB (1 MiB range) | 4.61 ms | 1.01 ms | **0.48 ms** | **9.6× faster** |
| peak under load memory | 588 MiB | 548 MiB | **155 MiB** | **3.8× lighter** |
| steady idle memory | 409 MiB | 356 MiB | **61.4 MiB** | **6.7× lighter** |

Per endpoint, same level and runs, Ferrofin is faster than Jellyfin 12.0-rc7 on all 28 compared requests, from 1.1× (`movies:items`) to 20.7× (`home:resume-book`); the full three-server table with p95/p99 is in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

⚠ marks a row where the servers did not do identical work, so the multiple is an indication rather than a like-for-like result:

1. detail: detail:item: missing MediaSources[].MediaStreams[].IsOriginal, MediaSources[].MediaStreams[].LocalizedOriginal, MediaStreams[].IsOriginal (+1 more)
2. playback: playback:playbackinfo: missing MediaSources[].MediaStreams[].IsOriginal, MediaSources[].MediaStreams[].LocalizedOriginal

Ferrofin implements the Jellyfin 10.11.8 contract, so a field that 12.0 added and 10.11.8 lacks shows up here as missing on both of them; the row is still Ferrofin's own work for everything else in it.

How steady are these numbers? Across the 3 runs, 3 of the 102 medians in the loaded-level tables moved by more than 15 % of their value, against 68 of the 204 p95/p99 tails, so read the p50s as the numbers and the tails as shape. Run-to-run ranges for every cell, and every place the servers did different work, are in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md); the harness and the one-sentence definition of each number are in [`bench/README.md`](bench/README.md). It is a deliberate, local instrument, not a CI job.
<!-- END GENERATED BENCHMARKS -->

## Quickstart

**Docker** (the release image bundles ffmpeg and the jellyfin-web client at `/web`):

```sh
docker run -d --name ferrofin \
  -p 8096:8096 \
  -v ferrofin-data:/data \
  -v /path/to/media:/media:ro \
  ghcr.io/mangoleaf/ferrofin:latest
```

**Helm** (the chart is published as an OCI artifact next to the image):

```sh
helm install ferrofin oci://ghcr.io/mangoleaf/ferrofin/charts/ferrofin -n ferrofin --create-namespace
```

See [`charts/ferrofin/README.md`](charts/ferrofin/README.md) and
[`values.example.yaml`](charts/ferrofin/values.example.yaml) for a worked configuration.

**From source** (needs the pinned Rust toolchain; ffmpeg is optional and its absence only
disables transcoding):

```sh
cargo run -p ferrofin-server -- --data-dir ./data --bind 127.0.0.1 --port 8096
```

On a fresh database Ferrofin seeds an `admin` user and **logs a generated password. Record
it**, or set `FERROFIN_ADMIN_PASSWORD` for a headless install. Then:

```sh
curl http://localhost:8096/System/Info/Public   # smoke test
# open http://localhost:8096/web, or point a native Jellyfin client at the server
```

Configuration is via CLI flags, `FERROFIN_*` environment variables, or
`{data_dir}/config.toml`, in that order of precedence. The full surface is in
[`docs/CONFIG.md`](docs/CONFIG.md).

## Migrating from Jellyfin

Ferrofin reads Jellyfin's database directly. Point it at a data directory containing a
Jellyfin 10.11.8 `jellyfin.db` and on first boot it detects the database, validates its
migration set (and refuses loudly rather than half-adopting an unexpected version), and
adopts it in place: **no re-scan, no re-import**. Users, watch state, playlists, and Live TV
configuration carry forward.

> ### ⚠ Migration is one-way. Back up first.
>
> Adopting the database runs Ferrofin's own migrations on it. Some of those rebuild
> Jellyfin's tables and normalise stored values, so the result is a Ferrofin database that
> Jellyfin is **not** guaranteed to open again. Ferrofin writes a copy of the original to
> `jellyfin.db.pre-ferrofin` before it touches anything and logs the path, but do not rely
> on that alone:
>
> 1. Stop Jellyfin.
> 2. **Copy the whole Jellyfin data directory somewhere safe** (the database, its `-wal`
>    and `-shm` files if present, and the metadata/config folders).
> 3. Start Ferrofin against a copy, not the original, until you are satisfied.
>
> If you decide to go back to Jellyfin, restore that backup. Anything that happened in
> Ferrofin after the switch (watch state, new users, playlists) stays in Ferrofin.

Existing Ferrofin databases upgrade in place across releases; steps that need operator
action are listed in [`docs/UPGRADING.md`](docs/UPGRADING.md).

## Plugins: the one deliberate divergence

Everything else in Ferrofin aims at parity with Jellyfin. Plugins are where it breaks
rank, on purpose.

**Jellyfin's model:** a plugin is a .NET assembly loaded into the server process with the
server's full privileges. Any plugin you install can read every file the server can,
open any network connection, and take the server down with it when it crashes. You are
trusting the plugin author with your machine.

**Ferrofin's model:** third-party code never runs with the server's privileges. There are
two tiers, and no third:

- **Compiled-in extensions** are Jellyfin plugins ported into this repository, reviewed
  like any other code and shipped inside the binary. They get full power *because* they
  went through review. Intro Skipper, File Transformation, and Merge Versions are ported
  today. They surface through the normal `/Plugins` API: dashboard entries, settings pages,
  enable/disable.
- **Sandboxed WASM plugins** are how code this repository has never seen gets installed:
  drop a `.wasm` component into `{data_dir}/plugins/` (or install from a configured plugin
  repository over HTTPS, checksum-verified) and restart. A WASM plugin runs with **no
  filesystem access, no network sockets, and no view of server memory**. Its whole world
  is a short, reviewable list of capabilities Ferrofin explicitly exports: logging, its own
  settings, host-mediated HTTP, read-only library queries, and writing its own media
  segments. Memory and CPU time are capped per plugin; one that misbehaves is interrupted
  and disabled while the server keeps serving. One artifact runs on every platform, and
  plugins can be written in any language that targets the WASM component model.

Be precise about what the sandbox does *not* remove: a plugin acting as a metadata source
can read your library's catalogue (titles, ids, file paths) and can ask Ferrofin to make
outbound HTTP requests on its behalf (destinations logged, bodies bounded, private and
loopback addresses refused unless you allowlist the plugin). So an actively malicious
plugin could still ship your movie list somewhere, and you should install plugins you have
some reason to trust. What it removes is the catastrophic tail every full-trust plugin
system carries: file access, raw sockets, and the run-anything blast radius.

Rust has no stable ABI, so a .NET-style native loader was never on the table, but that is
not why the design is this way. Ferrofin will not load untrusted native code into the
server process. Details and the capability list are in
**[`docs/EXTENSIONS.md`](docs/EXTENSIONS.md)**. To write one, clone
[`ferrofin-plugin-template`](https://github.com/mangoleaf/ferrofin-plugin-template): the
toolchain, target and contract bindings are already wired, and its CI publishes a plugin
repository you can add to a server to install it from the dashboard.

## Feature status

All 412 operations in the vendored Jellyfin 10.11.8 contract are implemented. Working
end-to-end: authentication, users and QuickConnect; library scan with live filesystem
watching; browse and query; images; sessions, playstate and remote control; WebSocket push;
playlists and collections; direct play and live HLS transcoding (NVENC, VAAPI and QSV
hardware paths); Live TV with M3U/XMLTV and DVR; SyncPlay; all 20 scheduled tasks;
metrics and tracing; trickplay, chapters, lyrics and media segments; photo and book
libraries; backup and restore.

Not implemented, by design: .NET-style native plugin loading (see above), DLNA server
discovery (SSDP), and the AMF, VideoToolbox, RKMPP and V4L2M2M hardware transcode paths
(unverifiable without the hardware; selecting one falls back to software with a logged
warning). Remote metadata providers (TMDB, TVDB, MusicBrainz, TheAudioDB, fanart) are on
by default and gated per library exactly as in Jellyfin; OMDb needs an API key.

The tiered matrix with verification depth per area is
**[`docs/FEATURES.md`](docs/FEATURES.md)**.

## Observability

Ferrofin is built to be run under a modern monitoring stack, not tailed in a terminal.

- **Structured JSON logs** on stdout by default, one event per line with a level, target
  and typed fields (`item_id`, `user_id`, `task`, …). Set `FERROFIN_LOG_FORMAT=text` for
  the human-readable form when running interactively; `FERROFIN_LOG` takes the usual
  `RUST_LOG` filter syntax.
- **Prometheus metrics** at `/metrics` when `FERROFIN_ENABLE_METRICS=true`, using
  Jellyfin's own metric names (`http_*`, `process_*`) so existing dashboards keep
  working. Two ready-made Grafana dashboards and a scrape config are in
  [`contrib/metrics/`](contrib/metrics/).
- **OpenTelemetry traces** over OTLP: set `OTEL_EXPORTER_OTLP_ENDPOINT` and every request,
  library scan, transcode and scheduled task becomes a span. Off unless that variable is
  set; sampling is the storage knob.

Conventions for each are in [`docs/conventions/`](docs/conventions/).

## Architecture

A Rust workspace of about twenty crates, edition 2024. The load-bearing idea: clients
depend on Jellyfin's **API surface**, not its code, so the contract is the vendored
OpenAPI spec and everything behind it is designed idiomatically in Rust (traits as the
dependency-injection seam, no ported OOP object hierarchy, runtime sqlx on SQLite).

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

Crate map, the C#-to-Rust mapping, and the porting rules: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Contributing

Every change is gated behind `cargo fmt`, `clippy -D warnings`, the full test suite, and
an 80 % per-crate line-coverage floor. Perf-touching changes need a measured before/after.
See [`CONTRIBUTING.md`](CONTRIBUTING.md); the deeper operating guide is [`CLAUDE.md`](CLAUDE.md).

Security policy: [`SECURITY.md`](SECURITY.md). Changelog: [`CHANGELOG.md`](CHANGELOG.md).
Releasing: [`RELEASE.md`](RELEASE.md).

## License

**GPL-3.0-only.** Ferrofin is a source-level derivative of Jellyfin. The Jellyfin repository
is internally inconsistent on this point (the root `LICENSE` is GPL-2.0, but the library
`.csproj` packages being ported declare `GPL-3.0-only`); Ferrofin follows the
`GPL-3.0-only` metadata on the specific crates it derives from. See [`LICENSE`](LICENSE).
