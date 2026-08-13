# Extensions — Jellyfin plugins, the Ferrofin way

Jellyfin's plugin system loads **.NET assemblies at runtime**: you drop a `.dll` into the
plugins directory and the server reflects over it, instantiates the plugin type, and calls
into it through shared .NET interfaces. That mechanism has no Rust equivalent — there is no
stable ABI for loading arbitrary compiled Rust code into a running process — and Ferrofin
deliberately **does not fake one**. `/Plugins` install and uninstall are rejected, not
stubbed to look like they worked.

Instead, in-process plugins come in **two deliberate forms**, both surfaced through the
exact same `/Plugins` API that dashboards already speak:

1. **Compiled-in extensions** (this document) — Jellyfin plugins ported into the Ferrofin
   codebase, reviewed like any other code, shipped inside the binary. Full trust, full
   access to internal seams.
2. **WASM plugins** (below) — sandboxed `.wasm` components installed by dropping a file
   into `{data_dir}/plugins/`, for plugins the Ferrofin repo has never seen.

To a Jellyfin client, both look and behave like installed plugins: they appear in the
dashboard's plugin list, have settings pages, and can be enabled or disabled at runtime
without a restart.

## How a compiled-in extension works

Every extension implements one trait (`crates/ferrofin-extensions/src/lib.rs`):

```rust
pub trait Extension: Send + Sync {
    fn id(&self) -> Uuid;                                  // also its /Plugins id
    fn descriptor(&self) -> PluginDescriptor;              // name, version, description
    fn default_config(&self) -> Vec<u8>;                   // seed config on first run
    fn config_pages(&self) -> Vec<PluginConfigPage> { … }  // vendored settings page(s)
    fn tasks(&self, cx: &ExtensionContext) -> Vec<Arc<dyn ScheduledTask>>;
}
```

An extension receives **only manager trait objects**, never concrete types — an
`ExtensionContext` bundling the `LibraryManager`, `MediaSegmentManager`, `PluginManager`,
`MergeVersionsManager`, an optional audio `Fingerprinter`, and a per-extension cache dir. It
is the same dependency-injection seam the rest of the server uses (see
[`ARCHITECTURE.md`](ARCHITECTURE.md)), so an extension can do real work — scan the library,
persist media segments, register background tasks — without reaching into implementation
crates.

The curated set is registered **statically** in one place (`builtin_extensions()`); there is
no discovery, no filesystem scan, no dynamic loading. Consequences:

- **They surface through `/Plugins`.** `registered_plugins()` turns each extension into a
  plugin descriptor with its seed config, so jellyfin-web's dashboard shows them and links
  their settings page (served via `GET /web/ConfigurationPage`).
- **Runtime enable/disable, no restart.** The persisted enabled flag lives in the
  `PluginManager`; each extension self-gates on it (disabled → its routes return `404`, its
  scheduled tasks no-op).
- **Vendored settings pages.** The upstream plugin's HTML settings page is committed into
  `crates/ferrofin-extensions/assets/<name>/` at build time, so normal builds are hermetic
  (no network fetch). Refresh with `FERROFIN_REFRESH_PLUGIN_ASSETS=1 cargo build -p ferrofin-extensions`.
- **The `CanUninstall=true` quirk.** jellyfin-web gates the enable/disable toggle on the
  plugin reporting `CanUninstall=true`, so compiled-in extensions report `true` to surface
  the toggle — even though the actual uninstall call is still rejected (you can't uninstall
  code that's compiled in).

## The ported plugins

Three third-party Jellyfin plugins ship as extensions today. Each is a faithful port of a
pinned upstream revision; the pins and the full delta live in
[`PLUGINS_UPSTREAM.md`](PLUGINS_UPSTREAM.md).

| Extension | Upstream | Ported rev | What it does |
|---|---|---|---|
| **Intro Skipper** | [intro-skipper/intro-skipper](https://github.com/intro-skipper/intro-skipper) | `db09359` | Chromaprint audio-fingerprint intro/credit detection, exposed as media segments |
| **File Transformation** | [IAmParadox27/jellyfin-plugin-file-transformation](https://github.com/IAmParadox27/jellyfin-plugin-file-transformation) | `f4f01c3` | Transform-on-serve hook other extensions build on |
| **Merge Versions** | [danieladov/jellyfin-plugin-mergeversions](https://github.com/danieladov/jellyfin-plugin-mergeversions) | `e6f58d6` | Bulk-merge duplicate movie/episode versions into one item, two 24h tasks |

**Accepted divergences** (not bugs — do not "fix" them during an upstream sync): Ferrofin
models version groups solely through the `PrimaryVersionId` pointer rather than Jellyfin's
internal `OwnerId`/`LinkedAlternateVersions` machinery, which is representation, not API
surface. Intro Skipper reports unavailable when `fpcalc` (Chromaprint) is absent. The precise
per-plugin list is in [`PLUGINS_UPSTREAM.md`](PLUGINS_UPSTREAM.md).

## Adding an extension

The full workflow (trait seam → extension module → handlers → composition root → vendored
page → tests) is documented for contributors and agents in the `plan-plugin-port` and
`implement-plugin-port` skills under `.claude/skills/`.

## WASM plugins (Tier 1b)

Runtime-installable plugins are **WebAssembly components** implementing the
`ferrofin:plugin` world (`crates/ferrofin-wasm/wit/ferrofin-plugin.wit` — the single
source of truth). Install = drop the `.wasm` file into `{data_dir}/plugins/` and restart,
matching Jellyfin's restart-after-install flow. On boot each component is compiled,
interrogated for its identity (`descriptor`), seed config, and task list, and then
registered through the same plugin manager as compiled-in extensions — same dashboard
entry, same enable/disable toggle, same `/Plugins/{id}/Configuration` storage.

**The sandbox is the point.** A WASM plugin gets *no filesystem, no direct network, no
environment, no stdio* — its only capabilities are the functions the WIT `host` interface
explicitly exports, so that one file is the entire reviewable attack surface. Today that
surface is: `log`, `get-config` (its own config only), `http-fetch` (host-executed HTTP with
the destination logged and the response capped at the plugin's memory limit), `query-items`
(a small read-only item projection, max 1000 rows per call), and `write-media-segments`
(scoped to the plugin's own provider id — it can never touch another provider's or a user's
segments). Plugins can also act as **metadata sources**: the scan offers every item to each
enabled plugin's `metadata-lookup` export after the built-in providers (NFO/TVDB/TMDB/OMDb)
ran, and applies results **supplement-only** — a plugin fills fields that are still empty
and records its own external ids; it can never overwrite a built-in provider or a user
edit. Each plugin runs on its own runtime thread under an enforced
per-call deadline (`FERROFIN_WASM_CALL_TIMEOUT_SECS`, default 30 s) and linear-memory cap
(`FERROFIN_WASM_MEMORY_LIMIT_MB`, default 128 MiB). A trap or overrun fails that one call
and the instance is rebuilt; three consecutive failures trip a circuit breaker that
sidelines the plugin until restart. The server never goes down with a plugin.

### What the sandbox does — and does not — protect against

Be precise about the security model; both halves matter.

**What a WASM plugin can never do**, no matter how malicious, because the sandbox has no
way to express it: read or write **any file** (your media, `system.json`, the SQLite
database with its password hashes, host SSH keys — nothing); open **its own network
connections**; execute host code, spawn processes, or read server memory; exceed its memory
and CPU-time limits; or take the server down (traps are contained, repeat offenders are
sidelined). This is the catastrophic tail of the traditional full-trust plugin model —
where any installed plugin runs with the server's own privileges — and it is gone entirely.

**What the capability surface deliberately grants**, and therefore what a malicious plugin
could still abuse: `query-items` exposes your library **catalog** (titles, ids, filesystem
paths — never file contents), and `http-fetch` performs outbound HTTP on the plugin's
behalf (destination logged, body bounded). Combined, a hostile plugin could send your movie
list to a remote host. By default `http-fetch` is refused for **private, loopback, link-local, and CGNAT
destinations** (your LAN, cloud metadata services, Tailscale/tailnet ranges, Ferrofin
itself), which
removes the server-as-network-pivot risk; grant a specific trusted plugin private-network
access with `FERROFIN_WASM_PRIVATE_HTTP_ALLOW` (comma-separated plugin UUIDs, or `*`).
Known limitation: the private-address check resolves-then-fetches, so a DNS-rebinding
attacker has a theoretical TOCTOU window; pinning the vetted address on the request is the
planned hardening. (Referencing plugins by UUID is also acknowledged UX debt — accepting
plugin names is a planned improvement.)

The one-line summary: **the sandbox bounds the blast radius to your catalog metadata; it
does not make strangers trustworthy.** Install plugins you have some reason to trust.

**What the memory limit means (and costs).** The 128 MiB is a **per-plugin ceiling, not an
allocation**: it is the point past which a plugin's `memory.grow` is refused (and the size
past which an `http-fetch` response is rejected). Nothing reserves that memory. Measured
real usage (printed by the `wasm_hello_guest` test on every CI run): with no plugins
installed the host costs almost nothing; the **first** loaded plugin pages in wasmtime's
JIT machinery, ~58 MiB **once per server process**; each **additional** plugin adds only a
few MiB (the 49 KiB reference plugin measures ~6 MiB as an upper bound). So ten
well-behaved plugins cost on the order of 120 MiB total — not 10 × 128 MiB. Raise the
limit only for plugins that legitimately hold large working sets (e.g. interpreter-language
guests bundling their runtime).

Plugins receive server events (`LibraryChanged`, `PlaybackStart`, task completions, …)
through the `on-event` export via a bounded per-plugin queue
(`FERROFIN_WASM_EVENT_QUEUE_CAPACITY`, default 256) — a slow guest loses events rather
than slowing the server; the database, not the event stream, is the source of truth.

Authoring: any language that targets the WASM component model. The reference plugin is
`examples/wasm-hello/` — a ~70-line Rust crate (`wit-bindgen` + `wasm32-wasip2`, its own
toolchain island) that logs a config-driven greeting from a scheduled task and counts
events. Build it with `cargo build --release --target wasm32-wasip2` inside that
directory. `.wasm` artifacts are never committed to this repo; CI builds the example from
source on every run so the WIT contract cannot drift silently.

**Contract stability:** the world is `0.x` and explicitly unstable until a few real
third-party plugins exist; after that it freezes like the OpenAPI contract (additive
evolution only). Planned capability growth (host-mediated HTTP, read-only item queries,
media-segment writes, then a metadata-provider export) is tracked in
`brain/plans/PLAN_PLUGIN_TIERS.md` phases E2–E3.

## Roadmap

- **WASM image contribution** — `metadata-lookup` deliberately excludes artwork in 0.x;
  the image-candidate contract lands when a real plugin needs it (the image pipeline's
  cache/dimension/blurhash integration deserves its own design pass).
- **External integrations over REST** — an event push story (webhooks / WebSocket
  subscriptions) for tools that are already separate systems (Jellyseerr-shaped). Planned
  as Tier 2; not part of the in-process plugin model.
