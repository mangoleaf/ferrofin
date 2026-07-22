# Contracts

Vendored, pinned copies of the **Jellyfin OpenAPI spec** — the authoritative HTTP contract
that TV clients (Wolphin, Swiftfin, Findroid) depend on. `hermit-api` is built to satisfy
this surface, and a Wave 7 CI test diffs `hermit-api`'s generated spec against it (Hermit
must be a superset; drift fails CI).

## `jellyfin-openapi-10.11.8.json`

- **Source:** pulled live from the homelab Jellyfin (`kubectl -n jellyfin`, `GET /api-docs/openapi.json`)
  on 2026-07-22. OpenAPI 3.0.1, 337 paths, 2.1 MB.
- **How Jellyfin produces it:** Swashbuckle.AspNetCore generates it at runtime from the
  controllers — it is NOT a static file in the source repo. (Upstream CI regenerates it by
  running the `OpenApiSpecTests` integration test.) We captured it from the running server
  instead, needing zero .NET toolchain on the host.

### Version note (important)

- **This spec = Jellyfin 10.11.8** (the released version running in the homelab — the exact
  API the real TV client talks to).
- **The source clone being ported = v12.0.0** (a dev snapshot at `~/dev/3rdparty/jellyfin`).

This is intentional: the goal is client compatibility, so the contract to satisfy is the API
the **client** uses (10.11.8), not the dev snapshot's. The v12 source is only what we
transliterate *from*. Any endpoint drift between v12 controllers and the 10.11.8 surface is
caught by the Wave 7 contract-diff gate and reconciled then. If we later target a different
client version, drop its spec here and repin.
