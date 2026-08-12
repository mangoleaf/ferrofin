# Configuration

Ferrofin is configured three ways, **highest precedence first**:

1. **CLI flags** — `--data-dir`, `--bind`, `--port`, `--published-url`, `--ffmpeg`, `--config`.
2. **`FERROFIN_*` environment variables** — the full surface, below.
3. **`config.toml`** — an optional file at `{data_dir}/config.toml` (or `--config`/
   `FERROFIN_CONFIG_FILE`). Keys are the env-var stems, lower-cased and prefix-stripped
   (`FERROFIN_BIND_ADDR` → `bind_addr`). The whole file is optional; a missing one is not an
   error.

Everything below has a working default except where noted; a bare `cargo run -p ferrofin-server`
boots.

## Paths

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_DATA_DIR` | `$XDG_DATA_HOME/ferrofin`, else `/var/lib/ferrofin` | Program-data root; derives the sub-dirs below and holds the database. |
| `FERROFIN_CONFIG_DIR` | `{data_dir}/config` | `system.json` + `branding.json`. |
| `FERROFIN_CACHE_DIR` | `{data_dir}/cache` | Cache root, including the transcode/segment cache. |
| `FERROFIN_WEB_DIR` | `{data_dir}/web` | Built jellyfin-web `dist/` served at `/web`. Absent → API-only. |
| `FERROFIN_CONFIG_FILE` | `{data_dir}/config.toml` | Path to the optional bootstrap `config.toml`. |

The database file is `{data_dir}/ferrofin.db`. For drop-in adoption Ferrofin also opens a
legacy `hermit.db` or a Jellyfin `jellyfin.db` (root or `data/jellyfin.db`) found in the data
dir — see [Migrating from Jellyfin](../README.md#migrating-from-jellyfin).

## Network & identity

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_BIND_ADDR` | `0.0.0.0` | Address the HTTP listener binds to. |
| `FERROFIN_PORT` | `8096` | HTTP port. |
| `FERROFIN_HTTPS_PORT` | `8920` | HTTPS port (TLS termination is deferred; parity value). |
| `FERROFIN_PUBLISHED_URL` | auto-detect | Public base URL advertised to clients. |
| `FERROFIN_BASE_URL` | none | URL path prefix the server is mounted under. |
| `FERROFIN_SERVER_NAME` | host name | Server name reported to clients. |
| `FERROFIN_LOG` | `info` | Log filter, `RUST_LOG`/`EnvFilter` syntax. |

## Admin seeding (fresh database only)

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_ADMIN_USER` | `admin` | Username of the admin seeded on first boot. |
| `FERROFIN_ADMIN_PASSWORD` | generated & logged | Admin password. Set it for a headless install; otherwise Ferrofin generates one and **logs it once — record it**. |

## ffmpeg

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_FFMPEG_PATH` | auto-discovered on `$PATH` | Path to `ffmpeg`. Absent ffmpeg only disables transcode. |
| `FERROFIN_FFPROBE_PATH` | auto-discovered on `$PATH` | Path to `ffprobe` (scan probes). |

## Library & database

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_LIBRARY_ROOTS` | none | Library root paths to seed on a fresh install. |
| `FERROFIN_DB_POOL` | `auto` | SQLite connection count, or `auto` (sizes to cores — the measured optimum). |
| `FERROFIN_SCAN_PROGRESS_EVERY` | built-in | Items between scan-progress log lines. |

## Observability

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_ENABLE_METRICS` | `false` | Serve Prometheus `/metrics`. Off → the route is absent. |
| `FERROFIN_METRICS_SAMPLE_INTERVAL` | built-in | Seconds between process-metric samples. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | none | Enable OTLP trace export to this endpoint (off by default). |

## Remote metadata providers (feature-gated, off by default)

These are read only when the provider feature is compiled in; without a key the provider
returns empty results. Never send user PII to these services.

| Variable | Purpose |
|---|---|
| `FERROFIN_OMDB_KEY` | OMDb API key (Rotten Tomatoes ratings). |
| `FERROFIN_TVDB_KEY` / `FERROFIN_TVDB_PIN` | TheTVDB API key + subscriber PIN. |
| `FERROFIN_FANART_KEY` | fanart.tv personal API key. |
| `FERROFIN_MUSICBRAINZ_URL` | MusicBrainz base URL override (self-hosted mirror). |
| `FERROFIN_STUDIOS_REPO_URL` | Studio-images repo URL override. |

## Build- and test-time only

Not runtime config, listed for completeness:

- `FERROFIN_REFRESH_PLUGIN_ASSETS=1` — re-fetch vendored extension settings pages during
  `cargo build -p ferrofin-extensions`.
- `FERROFIN_FFMPEG_TESTS=1` — run the env-gated real-ffmpeg integration tests.
