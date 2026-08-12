# Third-party plugin upstream tracking

One row per Jellyfin plugin ported into Hermit as a compiled-in extension
(`crates/hermit-extensions/`). This is the single source of truth for *which
upstream revision each port is based on*. The `sync-plugin-upstream` skill
(`.claude/skills/sync-plugin-upstream/`) parses this file, fetches each clone,
and ports behavioral deltas when upstream moves.

Conventions:
- **Clone** — local working copy under `~/dev/3rdparty/` (clone it if missing).
- **Ported rev** — the upstream commit the Hermit implementation is faithful to.
  Bump it only after the delta is actually ported (or classified as
  not-applicable/accepted-divergence below).
- Dashboard-asset pins live in `crates/hermit-extensions/build.rs`
  (`*_REPO`/`*_REV` consts) and must be kept equal to **Ported rev**.

| Plugin | Upstream repo | Clone | Ported rev | Upstream version | Status |
|---|---|---|---|---|---|
| Intro Skipper | https://github.com/intro-skipper/intro-skipper | `~/dev/3rdparty/intro-skipper` | `db09359` | 10.11/prerelease | ported |
| File Transformation | https://github.com/IAmParadox27/jellyfin-plugin-file-transformation | `~/dev/3rdparty/jellyfin-plugin-file-transformation` | `f4f01c3` | 2.5.10.0 | ported |
| Merge Versions | https://github.com/danieladov/jellyfin-plugin-mergeversions | `~/dev/3rdparty/jellyfin-plugin-mergeversions` | `e6f58d6` | 12.0.0 | ported |

## Per-plugin notes

### Intro Skipper
- Hermit files: `crates/hermit-extensions/src/intro_skipper.rs`,
  `crates/hermit-extensions/src/fingerprint.rs`,
  `crates/hermit-api/tests/intro_skipper_handlers.rs`,
  vendored assets `crates/hermit-extensions/assets/introskipper/`.
- Phase 5 (its own API routes) deliberately deferred — see
  the extension seam docs (`crates/hermit-extensions/src/lib.rs`).

### File Transformation
- Hermit files: `crates/hermit-extensions/src/file_transformation.rs`,
  vendored assets `crates/hermit-extensions/assets/filetransformation/`.

### Merge Versions
- Hermit files: `crates/hermit-extensions/src/merge_versions.rs` (the whole
  plugin — `MergeVersionsExtension`, `MergeVersionsService`, both scheduled
  tasks, config, eligibility filters), vendored assets
  `crates/hermit-extensions/assets/mergeversions/`, the trait seam
  `crates/hermit-traits/src/merge_versions.rs`, and the thin HTTP handlers
  `crates/hermit-api/src/handlers/merge_versions.rs`. The single-group
  `merge_versions`/`remove_alternate_sources` core ops (backing
  `POST /Videos/MergeVersions` + `DELETE /Videos/{id}/AlternateSources`) stay
  in `crates/hermit-core/src/library_manager.rs` — those are core Jellyfin
  routes, not plugin surface.
- Ported at 12.0 semantics (`e6f58d6`): provider-first episode merge key
  (Tvdb→Tmdb→Imdb → numbers → title, case-insensitive), transitive
  version-group expansion, existing-primary-preserving primary selection,
  `LocationsExcluded` config + inactive-library eligibility filters, the two
  24-hour dashboard tasks, and the vendored settings page. Routes and tasks
  self-gate on the plugin's enabled flag (disabled → routes 404, tasks no-op).
- Accepted divergences (do NOT "fix" during sync): Hermit models version
  groups solely via the `PrimaryVersionId` pointer — the C# `OwnerId` /
  `LocalAlternateVersions` / `LinkedAlternateVersions` / linked-child-reroute
  machinery is Jellyfin-internal representation, not API surface. No
  `VideoType`/`Video3DFormat` columns in Hermit's schema, so primary selection
  cannot demote 3D/non-file videos (width ordering only). No `IndexNumberEnd`
  column, so that episode-key component is always empty. Upstream's
  `Parallel.ForEach` fire-and-forget async merges (error-dropping) run
  sequentially in Hermit.
