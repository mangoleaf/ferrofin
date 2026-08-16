# Extensions — Jellyfin plugins, the Ferrofin way

Jellyfin's plugin system loads **.NET assemblies at runtime**: you drop a `.dll` into the
plugins directory and the server reflects over it, instantiates the plugin type, and calls
into it through shared .NET interfaces. That mechanism has no Rust equivalent — there is no
stable ABI for loading arbitrary compiled Rust code into a running process — and Ferrofin
deliberately **does not fake one**. What the `/Plugins` install API installs instead is a
sandboxed WASM component, never a native assembly.

Instead, in-process plugins come in **two deliberate forms**, both surfaced through the
exact same `/Plugins` API that dashboards already speak:

1. **Compiled-in extensions** (this document) — Jellyfin plugins ported into the Ferrofin
   codebase, reviewed like any other code, shipped inside the binary. Full trust, full
   access to internal seams.
2. **WASM plugins** (below) — sandboxed `.wasm` components installed from a plugin
   repository via the dashboard (or by dropping a file into `{data_dir}/plugins/`), for
   plugins the Ferrofin repo has never seen.

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
surface. Intro Skipper fingerprints with `ffmpeg -f chromaprint` (jellyfin-ffmpeg is built with
that muxer), falling back to `fpcalc`, and reports unavailable when neither exists. The precise
per-plugin list is in [`PLUGINS_UPSTREAM.md`](PLUGINS_UPSTREAM.md).

## Adding an extension

The full workflow (trait seam → extension module → handlers → composition root → vendored
page → tests) is documented for contributors and agents in the `plan-plugin-port` and
`implement-plugin-port` skills under `.claude/skills/`.

## WASM plugins (Tier 1b)

Runtime-installable plugins are **WebAssembly components** implementing the
`ferrofin:plugin` world (`crates/ferrofin-wasm/wit/ferrofin-plugin.wit` — the single
source of truth). Install matches Jellyfin's repository flow: an admin adds a plugin
repository URL (Dashboard → Plugins → Repositories), picks the plugin from the catalog,
and the server downloads, verifies, and stages it into `{data_dir}/plugins/`; a restart
activates it (`SystemInfo.HasPendingRestart` flips true). Dropping the `.wasm` file into
`{data_dir}/plugins/` by hand and restarting works identically (the dev/air-gapped path).
The repository install pipeline is deliberately strict, in order: the manifest's
`targetAbi` must equal this server's plugin ABI; the download must be **HTTPS**
(cleartext-to-loopback allowed only when the *configured repository* is itself loopback —
a remote manifest can't point the server at localhost), redirects re-checked per hop; the
size is capped while streaming (`FERROFIN_MAX_PLUGIN_DOWNLOAD_MB`, default 128 MiB); the
checksum must match (the manifest's `sha256` extension field preferred, else the
Jellyfin-standard MD5 — integrity, not authenticity: HTTPS is the trust root); and the
artifact must validate as a real `ferrofin:plugin` component whose self-reported id equals
the catalog guid. **Re-installing a version you already vetted is safe**: the sha256 of every
installed artifact is recorded, and re-installing a known version whose bytes changed is
refused — a compromised repository cannot silently swap the artifact under a version this
server already installed. (It can still publish a *new* malicious version; there is no
code signing, and the repository remains the trust root — install from repositories you
trust.) Install,
uninstall, enable/disable, configuration
writes, and repository changes require an **administrator** (Jellyfin's
`RequiresElevation`) — a plugin's config JSON is handed to the guest, so a
config write is guest input. On boot each component is compiled,
interrogated for its identity (`descriptor`), seed config, and task list, and then
registered through the same plugin manager as compiled-in extensions — same dashboard
entry, same enable/disable toggle, same `/Plugins/{id}/Configuration` storage.

**"Disabled" and load.** Enable/disable governs the runtime work — tasks don't run, events
aren't delivered, and metadata lookups are skipped for a disabled plugin. But a disabled
plugin's *identity* exports (`descriptor`/`default-config`/`tasks`) still run at every boot,
because that is how it appears in `/Plugins` at all. Those exports are metadata-only and
cannot reach the network — `http-fetch` (like `query-items` and `write-media-segments`) is
refused during load — so a disabled or newly-dropped-in plugin cannot phone home at startup.
To stop a plugin's code from running entirely, remove its `.wasm` and restart.

**Settings pages.** A plugin ships its own dashboard settings page(s) via the
`config-pages` export: raw HTML in the standard jellyfin-web plugin-page shape
(`data-role="page"` root + inline script against the `ApiClient`/`Dashboard`
globals), surfaced on `GET /web/ConfigurationPages` tagged with the plugin's id
— exactly how jellyfin-web decides to show the Settings button — and saved with
`ApiClient.updatePluginConfiguration` (the `/Plugins/{id}/Configuration` JSON
the plugin reads back through `get-config`). A plugin that ships **no** page
still gets one: Ferrofin synthesizes a generic JSON editor over its config, so
every WASM plugin is configurable from the dashboard. **Disabling a plugin removes its pages** from
discovery and fetch alike — the kill switch disarms the browser-side surface
too, matching Jellyfin (a disabled plugin is never instantiated there). One
caveat belongs in the trust section below: page content runs in the admin's
browser, not in the sandbox.

**The sandbox is the point.** A WASM plugin gets *no filesystem, no direct network, no
environment, no stdio* — its only capabilities are the functions the WIT `host` interface
explicitly exports, so that one file is the entire reviewable attack surface. Today that
surface is: `log`, `get-config` (its own config only), `http-fetch` (host-executed HTTP with
the destination logged, the response capped at the plugin's memory limit, and —
crucially — **gated by the plugin's own declared egress allowlist**, below), `query-items`
(a read-only item projection — filterable by kind/genre/user state, sortable, user-scopable
so parental limits apply and per-user played/favorite/resume fields populate; max 1000 rows
per call), `next-up` (the user's next-episodes queue), `get-state`/`set-state` (a small
per-plugin key/value store — 256 B keys, 1 MiB values, 8 MiB total by default
(`FERROFIN_WASM_STATE_LIMIT_MB`) — for per-user settings and cursors; the admin never
sees it), and `write-media-segments` (scoped to the plugin's
own provider id — it can never touch another provider's or a user's segments).

**Declared egress (`declared-egress`)** — every plugin ships its own public-network
allowlist inside the artifact (and, for template-built plugins, in plain sight in the
repo's `Cargo.toml`): exact hosts, `*.sub.example` wildcards, or `*` for plugins whose
destinations are user-configured (the server logs `*` plugins loudly at load).
**Deny-by-default: an empty list means no internet access at all** — most plugins
declare nothing and physically cannot phone home. The check runs on the URL's host
string *before any DNS resolution* (a denied fetch must not leak data through the DNS
query itself); private/LAN destinations remain a separate, admin-granted layer
(`FERROFIN_WASM_PRIVATE_HTTP_ALLOW`), which supersedes the declared list for plugins
the admin explicitly trusted — note the blast radius: naming a plugin there (or `*`)
exempts it from the declared-egress model entirely, public destinations included. Install-time validation records each plugin's declared
list, and an upgrade that GROWS it is warned about by name — a plugin's reach changing
is a decision-worthy event.

**Media analysis (`media-info` / `extract-audio` / `extract-frames` + the
`scan-targets`/`scan-media` exports)** — the generic analysis surface: **the host
decodes, the guest analyzes**. A plugin names a library ITEM (never a path — the host
resolves files itself and owns the whole decoder invocation), and receives bounded
decoded data: audio windows ≤ 60 s and ≤ a quarter of the plugin's memory limit as
PCM, or ≤ 16 sampled stills ≤ 320 px per call. Fingerprinting, loudness, silence and
black-frame detection — and analyses nobody has written yet — are all guest code over
these windows; extraction runs under a global decode budget (`FERROFIN_WASM_ANALYSIS_CONCURRENCY`,
default a quarter of the cores) so plugin analysis never starves transcodes. Plugins that declare `scan-targets` are
offered each new matching item exactly once by the "Plugin media analysis" dashboard
task. Why a host-driven pass instead of each plugin polling `query-items` with its own
cursor (which the 0.3 surface already allowed): the offer-once watermark lives under a
`host:`-reserved state key a guest can neither read nor rewind (a self-managed cursor
could be rewound to re-burn the shared decode budget), disabled plugins are centrally
skipped, N analyzers appear as one dashboard task instead of N, and future scan
behaviors (changed-item re-offers, scan-completion triggering, progress) evolve
host-side without another ABI change. A guest error never fails a scan. Trust note: this grants
the guest **media content**, one rung above catalog metadata — leaving the sandbox
still requires declared egress, and a genuine analysis plugin declares `[]`.

Plugins can also OWN A URL SPACE and EXTEND THE WEB UI (the two capabilities that make
plugins like Home Screen Sections possible):

- **`handle-request`** — the server routes `ANY /Plugins/{id}/web/*` into the plugin
  (method, path, query, headers, body in; status, headers, body out), on the plugin's own
  runtime thread under the same deadline/memory/breaker discipline as every guest call.
  This URL space is reachable **without authentication** (plugin pages load their assets
  via plain `<script src>` tags, exactly like upstream `[AllowAnonymous]` plugin
  controllers); the caller's resolved identity (`user-id`, `is-admin`,
  `is-authenticated`) is forwarded — never a token — and the plugin gates its own
  sensitive paths. Inbound bodies are capped at 1 MiB; a disabled plugin's URL space 404s.
- **`web-transforms`** — declared literal search/replace patches the server applies to
  matching `/web` files while the plugin is enabled (capped: 16 per plugin, 256 KiB per
  text). This is how a plugin injects its client-side hooks into jellyfin-web.

Plugins can also act as **metadata and artwork providers**: the scan offers every item
to each enabled plugin's `metadata-lookup` export after the built-in providers
(NFO/TVDB/TMDB/OMDb) ran, and applies results **supplement-only** — a plugin fills fields
that are still empty and records its own external ids; it can never overwrite a built-in
provider or a user edit. A plugin that declares `provider-info` becomes a **named
provider**: its name appears in each library's *Metadata downloaders* / *Image fetchers*
checkboxes, and the per-library selection and order are enforced during the scan — for
named plugins and for the built-ins alike (TheTVDB vs TheMovieDb authority for a series
follows the saved order; a fetcher a library unchecked never runs for its items).
Artwork rides the `remote-images` export: for items still missing a Primary/Backdrop
after the built-in chain, the host asks each eligible plugin for **image candidates
(URLs)** and downloads the winner itself through that plugin's declared egress
(20 MiB cap, 30 s timeout, redirects off, private addresses refused) — raw image bytes
never enter guest memory, and an undeclared image host is refused before DNS. Each plugin runs on its own runtime thread under an enforced
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
list to a remote host — **but only to a destination it declared in its own auditable
egress allowlist**: read the plugin's `egress = [...]` before installing, and treat a `*`
declaration as the plugin asking for the whole internet. By default `http-fetch` is refused for **private, loopback, link-local, and CGNAT
destinations** (your LAN, cloud metadata services, Tailscale/tailnet ranges, Ferrofin
itself), which
removes the server-as-network-pivot risk; grant a specific trusted plugin private-network
access with `FERROFIN_WASM_PRIVATE_HTTP_ALLOW` (comma-separated plugin UUIDs, or `*`).
The private-address check also **pins the vetted address on the connection
itself** (the request resolves to the address that was checked, not a second
DNS answer), closing the classic DNS-rebinding TOCTOU. (Referencing plugins by
UUID in the allowlist is acknowledged UX debt — accepting plugin names is a
planned improvement.)

**Web transforms are JavaScript injection into EVERY user's browser.** A
plugin's declared `web-transforms` rewrite served jellyfin-web files for all
users — a strictly larger grant than settings pages (admin-only) or plugin
routes (opt-in fetches). It is the single strongest reason to install only
plugins you trust; a disabled plugin's transforms are not applied
(restart-required, like everything decided at boot).

**Settings pages are outside the sandbox.** A plugin's `config-pages` HTML/JS
executes in the **admin's browser with the admin's session** — the same trust
model as every Jellyfin plugin page, and the WASM sandbox does not apply to it:
a malicious page could drive any admin API your session can. The synthesized
fallback page is host-authored (guest strings are escaped), so a plugin that
ships no page adds no browser-side surface.

The one-line summary: **the sandbox bounds the blast radius to your catalog
metadata — except a plugin's own settings page, which runs in your browser; it
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

## Non-goals for the WASM tier

- **Authentication providers (SSO/LDAP)** — security-critical core surface; if wanted it
  becomes a core feature, never sandbox-hosted third-party code.
- **DLNA** — needs SSDP/UDP sockets; the sandbox has no sockets by design.
- **Item mutation/linking** (Merge-Versions-shaped) — item identity stays host-owned;
  plugins supplement, they never restructure the library.

## Roadmap

- **External integrations over REST** — an event push story (webhooks / WebSocket
  subscriptions) for tools that are already separate systems (Jellyseerr-shaped). Planned
  as Tier 2; not part of the in-process plugin model.
