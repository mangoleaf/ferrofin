<div align="center">

# Ferrofin

**A drop-in Rust implementation of the Jellyfin media server.**

[![License: GPL-3.0-only](https://img.shields.io/badge/license-GPL--3.0--only-blue.svg)](LICENSE)
[![Rust edition 2024](https://img.shields.io/badge/rust-edition%202024-orange.svg)](rust-toolchain.toml)
[![API surface: 412/412](https://img.shields.io/badge/API%20surface-412%2F412-brightgreen.svg)](docs/FEATURES.md)

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

- **Measured against Jellyfin, every release.** Each release is measured against Jellyfin on the same
  machine and library: speed, time to first screen and memory. [Results](#results) summarises the
  latest release; its own page graphs every comparable cell and lists the rest with the reason they are
  withheld.
- **One binary, no runtime.** A single `ferrofin-server` executable plus ffmpeg. No .NET
  runtime, no JIT warm-up. The Docker image bundles both jellyfin-ffmpeg and the jellyfin-web
  client, so `docker run` gives you the whole server, web UI included.
- **Drop-in compatible.** Same API contract, same on-disk database format (pinned to
  Jellyfin 12.0), same password hashes. Adopt an existing Jellyfin 10.11.8–10.11.11,
  12.0.0, 12.1.0 or 12.2.0 library with no re-scan; see [supported and tested versions](gates/adoption/README.md#supported-and-tested-versions).
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

## Quickstart

**Docker** (the release image bundles ffmpeg and the jellyfin-web client at `/web`):

```sh
docker run -d --name ferrofin \
  -p 8096:8096 \
  -v ferrofin-data:/data \
  -v /path/to/media:/media:ro \
  ghcr.io/mangoleaf/ferrofin:latest
```

For automatic LAN server discovery, see [server discovery deployment](docs/SERVER_DISCOVERY.md).
The TCP mapping above supports manual URL connections; it does not forward discovery broadcasts.

**Helm** (the chart is published as an OCI artifact next to the image):

```sh
helm install ferrofin oci://ghcr.io/mangoleaf/ferrofin/charts/ferrofin -n ferrofin --create-namespace
```

See [`charts/ferrofin/README.md`](charts/ferrofin/README.md) and
[`values.example.yaml`](charts/ferrofin/values.example.yaml) for a worked configuration.

**Debian/Ubuntu package** (depends on `jellyfin-ffmpeg8` and `jellyfin-web` from Jellyfin's
apt repository, ships a systemd unit): add the repository, then
`apt-get install ./ferrofin_<version>_<arch>.deb` from the releases page. The full walk-through,
including the plain release tarball, is [`docs/INSTALL.md`](docs/INSTALL.md).

**From source** (needs the pinned Rust toolchain; ffmpeg is optional and its absence only
disables transcoding):

```sh
cargo run -p ferrofin-server -- --data-dir ./data --bind 127.0.0.1 --port 8096
```

On a fresh database Ferrofin seeds an `admin` user **with no password**, as Jellyfin does,
and logs a warning: the setup wizard at `/web` sets the password. Until it does, anyone who
can reach the server can sign in as that administrator, so keep it bound to loopback (or,
for the container image, which listens on all interfaces, keep its port unpublished) until
setup is done, or set `FERROFIN_ADMIN_PASSWORD` to seed the password for a headless
install. Then:

```sh
curl http://localhost:8096/System/Info/Public   # smoke test
# open http://localhost:8096/web, or point a native Jellyfin client at the server
```

Configuration is via CLI flags, `FERROFIN_*` environment variables, or
`{data_dir}/config.toml`, in that order of precedence. The full surface is in
[`docs/CONFIG.md`](docs/CONFIG.md).

## Migrating from Jellyfin

Ferrofin reads Jellyfin's database directly. Point it at a data directory containing a
Jellyfin **10.11.8, 10.11.9, 10.11.10, 10.11.11, 12.0.0, 12.1.0 or 12.2.0** `jellyfin.db` and on first boot it detects the database,
validates its migration set (and refuses loudly rather than half-adopting an unexpected version), and
adopts it in place: **no re-scan, no re-import**. Users, watch state, playlists, and Live TV
configuration carry forward. Ferrofin retains the `Users.NormalizedUsername` column and
unique index from Jellyfin 10.11.10/10.11.11 and adds them to older databases. Login,
creation, and renaming use Unicode-aware invariant uppercase keys compatible with
Jellyfin, while the original spelling remains visible. If existing usernames collide
under these rules, startup stops with the conflicting account IDs and names; resolve
the names in the original server before retrying. Accounts are never merged.

Every release's gates session adopts each supported version and upgrade route, and the latest
release's status is in [Results](#adopting-an-existing-jellyfin-database). Checks cover API
summaries against Jellyfin 12.2, database integrity, and stable responses without repeated
repairs after restart. See the [support matrix and tested build](gates/adoption/README.md#supported-and-tested-versions).
Support is determined by the exact migration history; these version numbers do not imply
support for every 10.11.x or 12.x release.

> ### ⚠ Migration is one-way. Back up first.
>
> Adopting the database runs Ferrofin's own migrations on it. Some of those rebuild
> Jellyfin's tables and normalise stored values, so the result is a Ferrofin database that
> Jellyfin is **not** guaranteed to open again. Ferrofin writes a copy of the original to
> `jellyfin.db.pre-ferrofin` before it touches anything and logs the path, but do not rely
> on that alone:
>
> 1. Stop both Jellyfin and Ferrofin.
> 2. **Back up the whole Jellyfin data directory and all configuration files**, including
>    hidden files and database `-wal`/`-shm` companions if present. Configuration may live
>    separately, for example in `/etc/jellyfin` on Debian. Include any external state
>    directories too.
> 3. Copy Jellyfin's `data/jellyfin.db`, `data/playlists/`, `data/collections/`,
>    `root/default/` and `metadata/` to the same relative paths under Ferrofin's data
>    directory, and its XML configuration into Ferrofin's config directory, **before
>    Ferrofin's first startup**. [`scripts/migrate-jellyfin.sh`](scripts/migrate-jellyfin.sh)
>    does this for the Debian layout. Keep the originals and backup intact. An existing
>    `ferrofin.db` or Ferrofin JSON configuration takes precedence over copied Jellyfin
>    files.
>
> `root/default/` holds the library definitions and `metadata/` the images. On first start
> Ferrofin imports the supported settings from `system.xml`, `encoding.xml`, `branding.xml`
> and `network.xml` into JSON, once; omitting `network.xml` discards remote-access
> restrictions and IP filters. Verify the imported settings and investigate import
> warnings before exposing the server. Jellyfin .NET plugins are not copied and need
> Ferrofin-compatible replacements.
>
> Follow the [migration procedure](docs/INSTALL.md#migrate-an-existing-jellyfin-installation)
> for copying, startup verification, and rollback.
>
> If you decide to go back to Jellyfin, restore that backup. Anything that happened in
> Ferrofin after the switch (watch state, new users, playlists) stays in Ferrofin.

Existing Ferrofin databases upgrade in place across releases; steps that need operator
action are listed in [`docs/UPGRADING.md`](docs/UPGRADING.md).

## Results

<!-- BEGIN GENERATED RESULTS — do not edit by hand. Regenerate with: make publish-results (gates/publish.py) -->
### Speed and memory against Jellyfin

Ferrofin **v1.4.0** against **Jellyfin 12.2.0**: the median of 3 runs on one host (32 cores, 125 GiB), each server alone in its container, over the same library, 2026-10-26. One host and one library: other hardware and libraries give other numbers.

**12.1× faster cold start** · **2.2–4.1× the speed on 5 screens at 5 screens/s** · **2.6× lighter peak memory under load** · **2.6× lighter idle memory**

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/results/v1.4.0/screens-loaded-dark.svg">
  <img alt="Screens at 5 screens/s (loaded): p50 latency: Ferrofin against Jellyfin 12.2.0 on 5 comparable cells, as fast or faster on 5; from 2.2× faster to 4.1× faster." src="docs/results/v1.4.0/screens-loaded.svg">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/results/v1.4.0/startup-dark.svg">
  <img alt="Time to first screen: speed-up: Ferrofin against Jellyfin 12.2.0 on 3 comparable cells, as fast or faster on 3; from 2.3× faster to 12.1× faster." src="docs/results/v1.4.0/startup.svg">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/results/v1.4.0/memory-dark.svg">
  <img alt="Memory: Ferrofin against Jellyfin 12.2.0 on 2 comparable cells, as fast or faster on 2; from 2.6× lighter to 2.6× lighter." src="docs/results/v1.4.0/memory.svg">
</picture>

Not compared in this release, because the two servers did different work there or the evidence is incomplete: the search screen, the search endpoints, the image endpoints, CPU per screen. The search screen therefore has no comparison yet.

Measured from commit `5112240e`. Every graph and table, the withheld cells and the sources: [docs/results/v1.4.0/README.md](docs/results/v1.4.0/README.md); every number with its range: [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

### Adopting an existing Jellyfin database

Each supported Jellyfin version and upgrade route, adopted in place by Ferrofin **v1.4.0** (commit `5112240e1923`, 2026-10-26): **13 of 14 passed**. Adoption is one-way; see [Migrating from Jellyfin](#migrating-from-jellyfin). Every route: [docs/results/v1.4.0/README.md](docs/results/v1.4.0/README.md).

### Answering like Jellyfin

The parity gate replays the **2,353** requests jellyfin-web makes against Jellyfin 12.2.0 and Ferrofin **v1.4.0** (commit `5112240e`, 2026-10-26) and compares the answers. Of the 2,307 it could compare, **1,242 are identical** and **1,051 are known differences**, each recorded in the parity ledger with the issue that tracks it, or the parity row that accepts it; **2 are filed as issues** and **12 are new**. 46 were not compared because Jellyfin itself failed them. **The parity gate did not pass** (new differences): 14 requests differ in ways the ledger does not cover yet. Every API area: [docs/results/v1.4.0/README.md](docs/results/v1.4.0/README.md).

<!-- END GENERATED RESULTS -->

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
hardware paths); Live TV with M3U/XMLTV and DVR; SyncPlay; 20 scheduled tasks;
metrics and tracing; trickplay, chapters, lyrics and media segments; photo and book
libraries; backup and restore.

Not implemented, by design: .NET-style native plugin loading (see above), DLNA server
discovery (SSDP), and the AMF, VideoToolbox, RKMPP and V4L2M2M hardware transcode paths
(unverifiable without the hardware; selecting one falls back to software with a logged
warning). Remote metadata providers (TMDB, TVDB, MusicBrainz, TheAudioDB, fanart) are on
by default and gated per library exactly as in Jellyfin, including OMDb. Shared
provider keys are built in; operator keys are optional overrides.

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
an 80 % per-crate line-coverage floor. Performance and memory are benchmarked before each
release.
See [`CONTRIBUTING.md`](CONTRIBUTING.md); the deeper operating guide is [`CLAUDE.md`](CLAUDE.md).

Security policy: [`SECURITY.md`](SECURITY.md). Changelog: [`CHANGELOG.md`](CHANGELOG.md).
Releasing: [`RELEASE.md`](RELEASE.md).

## License

**GPL-3.0-only.** Ferrofin is a source-level derivative of Jellyfin. The Jellyfin repository
is internally inconsistent on this point (the root `LICENSE` is GPL-2.0, but the library
`.csproj` packages being ported declare `GPL-3.0-only`); Ferrofin follows the
`GPL-3.0-only` metadata on the specific crates it derives from. See [`LICENSE`](LICENSE).
