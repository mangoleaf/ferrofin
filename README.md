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

- **Faster movie browsing, lower memory.** Across three runs on the same machine and
  fixture, Ferrofin's movie-screen HTTP workload had **13 ms median latency against
  37 ms** for Jellyfin 12.0.0, and steady idle memory was **152 MiB against 423 MiB**.
  Other screen comparisons remain withheld where response validation differs.
  Results, ranges and limitations are in [Benchmarks](#benchmarks).
- **One binary, no runtime.** A single `ferrofin-server` executable plus ffmpeg. No .NET
  runtime, no JIT warm-up. The Docker image bundles both jellyfin-ffmpeg and the jellyfin-web
  client, so `docker run` gives you the whole server, web UI included.
- **Drop-in compatible.** Same API contract, same on-disk database format (pinned to
  Jellyfin 12.0), same password hashes. Adopt an existing Jellyfin 10.11.8–10.11.11,
  12.0.0 or 12.1.0 library with no re-scan; see [supported and tested versions](adoption/README.md#supported-and-tested-versions).
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
     python3 bench/report.py --readme README.md bench/runs/quiet-cpu-comparison-current bench/runs/quiet-cpu-comparison-run2 bench/runs/quiet-cpu-comparison-run3 -->
Ferrofin `quiet-cpu-comparison-current` against **Jellyfin 12.0.0** and **Jellyfin 10.11.8**, measured 2026-09-17 on one machine (AMD Ryzen 9 9950X3D), each server alone in a container pinned to 8 logical CPUs with an 8 GiB limit, over a fixture of 3001 movies, 250 series and 7490 episodes. Published figures are medians across 3 selected runs; incomplete or non-comparable cells are withheld.
No measured request failed in the recorded load windows (unloaded, loaded, stress).

The screen rows are scripted HTTP transactions based on jellyfin-web 10.11.8, replayed at 5 screens per second (the "loaded" level). They include API and poster requests, not browser rendering. Latency reads **p50 / p95 / p99 in milliseconds**; the last column compares p50 with Jellyfin 12.0.0.

| | Jellyfin 12.0.0 | Jellyfin 10.11.8 | **Ferrofin** | Ferrofin vs 12.0.0 |
|---|---|---|---|---|
| **home** screen | 45 / 48 / 52 | — ⚠[1] | **— ⚠[2]** | **—** |
| **movies** screen | 37 / 41 / 44 | 23 / 27 / 29 | **13 / 16 / 17** | **2.8× faster** |
| **detail** screen | 39 / 42 / 44 | — ⚠[3] | **— ⚠[4]** | **—** |
| **series** screen | 30 / 32 / 34 | — ⚠[5] | **— ⚠[6]** | **—** |
| **search** screen | 58 / 65 / 82 | — ⚠[7] | **— ⚠[7]** | **—** |
| **playback** screen | 31 / 34 / 45 | — ⚠[8] | **— ⚠[8]** | **—** |
| cold start (restart → authenticated views; host caches retained) | 2143 ms | 2079 ms | **244 ms** | **8.8× faster** |
| direct-play TTFB (1 MiB range) | 4.31 ms | 1.07 ms | **0.52 ms** | **8.3× faster** |
| peak under load memory | 571 MiB | 672 MiB | **202 MiB** | **2.8× lighter** |
| steady idle memory | 423 MiB | 481 MiB | **152 MiB** | **2.8× lighter** |

Among 22 comparable endpoint measurements at this level, Ferrofin is faster on 22, about the same on 0, and slower on 0 against Jellyfin 12.0.0. Flagged or missing endpoints are excluded from these counts. The full tables are in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

⚠ marks a withheld cell: its evidence is incomplete or the servers did different work. Raw diagnostic numbers remain in the full report.

1. Jellyfin 10.11.8: home: home:latest-music: ids differs for a request pick
2. Ferrofin: home: home:latest-shows: ids differs for a request pick
3. Jellyfin 10.11.8: detail: detail:image: request picks differ; measured request selections differ from validated slot
4. Ferrofin: detail: detail:image: request picks differ
5. Jellyfin 10.11.8: series: series:episodes: per-item fields/types differ for a request pick
6. Ferrofin: series: series:similar: ids differs for a request pick
7. Jellyfin 10.11.8: search, Ferrofin: search: search:artists: ids differs for a request pick
8. Jellyfin 10.11.8: playback, Ferrofin: playback: playback:playbackinfo: per-item fields/types differ for a request pick

How steady are these numbers? Across the 3 runs, 5 of the 117 medians in the loaded-level tables moved by more than 15 % of their value, against 85 of the 234 p95/p99 tails, so read the p50s as the numbers and the tails as shape. Run-to-run ranges for every cell, and every place the servers did different work, are in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md); the harness and the one-sentence definition of each number are in [`bench/README.md`](bench/README.md). It is a deliberate, local instrument, not a CI job.
<!-- END GENERATED BENCHMARKS -->

The measured server source is commit `9c17abad`; the run name above identifies the
local result set, not a release. CPU observations and the retained cold-start range
(240–1087 ms across Ferrofin's run medians) are in [the full results](docs/BENCHMARKS.md).
Whole-workload CPU savings claims remain withheld while response/work differences are
unresolved. The host passed the normal idle checks, but sampled interference remained.


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
apt repository, ships a hardened systemd unit): add the repository, then
`apt-get install ./ferrofin_<version>_<arch>.deb` from the releases page. The full walk-through,
including the plain release tarball, is [`docs/INSTALL.md`](docs/INSTALL.md).

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
Jellyfin **10.11.8, 10.11.9, 10.11.10, 10.11.11, 12.0.0 or 12.1.0** `jellyfin.db` and on first boot it detects the database,
validates its migration set (and refuses loudly rather than half-adopting an unexpected version), and
adopts it in place: **no re-scan, no re-import**. Users, watch state, playlists, and Live TV
configuration carry forward. Ferrofin retains the `Users.NormalizedUsername` column and
unique index from Jellyfin 10.11.10/10.11.11 and adds them to older databases. Login,
creation, and renaming use Unicode-aware invariant uppercase keys compatible with
Jellyfin, while the original spelling remains visible. If existing usernames collide
under these rules, startup stops with the conflicting account IDs and names; resolve
the names in the original server before retrying. Accounts are never merged.

All six supported releases passed the live adoption suite on 2026-09-16: seven fixture
paths, including both 10.11.8 → 12.1.0 and 10.11.8 → 12.0.0 → 12.1.0. Checks cover API
summaries against Jellyfin 12.1, database integrity, and stable responses without repeated
repairs after restart. See the [support matrix and tested build](adoption/README.md#supported-and-tested-versions).
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
> 3. Copy the complete data and configuration into a clean Ferrofin destination **before
>    its first startup**. Keep the originals and backup intact. An existing `ferrofin.db`
>    or Ferrofin JSON configuration takes precedence over copied Jellyfin files.
>
> The complete copy preserves library definitions under `root/default/`, images under
> `metadata/`, and server configuration. In particular, omitting `network.xml` can discard
> remote-access restrictions and IP filters. Ferrofin imports supported XML settings into
> JSON; verify the imported settings and investigate import warnings before exposing the
> server. Jellyfin .NET plugins need Ferrofin-compatible replacements.
>
> Follow the [migration procedure](docs/INSTALL.md#migrate-an-existing-jellyfin-installation)
> for copying, startup verification, and rollback.
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
