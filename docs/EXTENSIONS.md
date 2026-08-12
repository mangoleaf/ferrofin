# Extensions — Jellyfin plugins, the Ferrofin way

Jellyfin's plugin system loads **.NET assemblies at runtime**: you drop a `.dll` into the
plugins directory and the server reflects over it, instantiates the plugin type, and calls
into it through shared .NET interfaces. That mechanism has no Rust equivalent — there is no
stable ABI for loading arbitrary compiled Rust code into a running process — and Ferrofin
deliberately **does not fake one**. `/Plugins` install and uninstall are rejected, not
stubbed to look like they worked.

Instead, the plugins that matter are **compiled into the server as first-class extensions**,
and surfaced through the exact same `/Plugins` API that dashboards already speak. To a
Jellyfin client, a Ferrofin extension looks and behaves like an installed plugin: it appears
in the dashboard's plugin list, has a settings page, and can be enabled or disabled at
runtime without a restart.

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

## Roadmap

Compiled-in extensions cover the plugins people actually run, but they require a rebuild to
add or change. Two future directions extend the model without that cost:

- **WASM component host** — load extensions as WebAssembly components (WIT-typed against the
  manager traits) so third parties can ship capabilities without a fork or a server rebuild.
- **External integrations over REST** — an event bus that pushes server events to external
  services (webhooks / WebSocket), for integrations that live outside the process entirely.

Both are design directions, not shipped features; the compiled-in `Extension` trait is what
exists today.
