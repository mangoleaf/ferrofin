# Third-party plugin upstream tracking

One row per Jellyfin plugin ported into Ferrofin as a compiled-in extension
(`crates/ferrofin-extensions/`). This is the single source of truth for *which
upstream revision each port is based on*. The `sync-plugin-upstream` skill
(`.claude/skills/sync-plugin-upstream/`) parses this file, fetches each clone,
and ports behavioral deltas when upstream moves.

Conventions:
- **Clone** — local working copy under `~/dev/3rdparty/` (clone it if missing).
- **Ported rev** — the upstream commit the Ferrofin implementation is faithful to.
  Bump it only after the delta is actually ported (or classified as
  not-applicable/accepted-divergence below).
- Dashboard-asset pins live in `crates/ferrofin-extensions/build.rs`
  (`*_REPO`/`*_REV` consts) and must be kept equal to **Ported rev**.

| Plugin | Upstream repo | Clone | Ported rev | Upstream version | Status |
|---|---|---|---|---|---|
| Intro Skipper | https://github.com/intro-skipper/intro-skipper | `~/dev/3rdparty/intro-skipper` | `db09359` | 10.11/prerelease | ported |
| File Transformation | https://github.com/IAmParadox27/jellyfin-plugin-file-transformation | `~/dev/3rdparty/jellyfin-plugin-file-transformation` | `f4f01c3` | 2.5.10.0 | ported |
| Merge Versions | https://github.com/danieladov/jellyfin-plugin-mergeversions | `~/dev/3rdparty/jellyfin-plugin-mergeversions` | `e6f58d6` | 12.0.0 | ported |

## Per-plugin notes

### Intro Skipper
- Ferrofin files: `crates/ferrofin-extensions/src/intro_skipper.rs`,
  `crates/ferrofin-extensions/src/fingerprint.rs`,
  `crates/ferrofin-api/tests/intro_skipper_handlers.rs`,
  vendored assets `crates/ferrofin-extensions/assets/introskipper/`.
- Phase 5 (its own API routes) deliberately deferred — see
  the extension seam docs (`crates/ferrofin-extensions/src/lib.rs`).

### File Transformation
- Ferrofin files: `crates/ferrofin-extensions/src/file_transformation.rs`,
  vendored assets `crates/ferrofin-extensions/assets/filetransformation/`.

### Merge Versions
- Ferrofin files: `crates/ferrofin-extensions/src/merge_versions.rs` (the whole
  plugin — `MergeVersionsExtension`, `MergeVersionsService`, both scheduled
  tasks, config, eligibility filters), vendored assets
  `crates/ferrofin-extensions/assets/mergeversions/`, the trait seam
  `crates/ferrofin-traits/src/merge_versions.rs`, and the thin HTTP handlers
  `crates/ferrofin-api/src/handlers/merge_versions.rs`. The single-group
  `merge_versions`/`remove_alternate_sources` core ops (backing
  `POST /Videos/MergeVersions` + `DELETE /Videos/{id}/AlternateSources`) stay
  in `crates/ferrofin-core/src/library_manager.rs` — those are core Jellyfin
  routes, not plugin surface.
- Ported at 12.0 semantics (`e6f58d6`): provider-first episode merge key
  (Tvdb→Tmdb→Imdb → numbers → title, case-insensitive), transitive
  version-group expansion, existing-primary-preserving primary selection,
  `LocationsExcluded` config + inactive-library eligibility filters, the two
  24-hour dashboard tasks, and the vendored settings page. Routes and tasks
  self-gate on the plugin's enabled flag (disabled → routes 404, tasks no-op).
- Accepted divergences (do NOT "fix" during sync): Ferrofin models version
  groups solely via the `PrimaryVersionId` pointer — the C# `OwnerId` /
  `LocalAlternateVersions` / `LinkedAlternateVersions` / linked-child-reroute
  machinery is Jellyfin-internal representation, not API surface. No
  `VideoType`/`Video3DFormat` columns in Ferrofin's schema, so primary selection
  cannot demote 3D/non-file videos (width ordering only). No `IndexNumberEnd`
  column, so that episode-key component is always empty. Upstream's
  `Parallel.ForEach` fire-and-forget async merges (error-dropping) run
  sequentially in Ferrofin. The episode merge key is scoped to the series
  *row* (`SeriesPresentationUniqueKey`), not the series name as upstream
  does: a show present in two libraries (hot/cold tiers) has two series rows
  with one name, and the name-scoped key merged episodes across them —
  hiding each alternate from its own series' list and skewing season counts.
  The bulk episode task self-heals links whose key no longer matches their
  primary's (unlink + regroup within the series). For the same reason movie
  grouping is keyed by (owning library `TopParentId`, `Tmdb` id) rather than
  the `Tmdb` id alone: the same film held in a cold (NAS) and a hot (local)
  library is two intentional entries, and merging them hid one behind the
  other. The bulk movie task self-heals existing cross-library links between
  `Tmdb`-carrying movies, and a group's `GetAllAlternateVersions` expansion
  drops any member it reaches outside that group's library, so a partner that
  never reaches the scan (excluded location, inactive library) cannot be
  re-merged through a stale pointer; such a member is unlinked only when it
  points back into the scanned library, never when its own primary is also
  outside (that is the other library's group, which this scan could not
  repair). Accepted cost, deliberately not configurable: the
  separate-4K-library setup that upstream's Tmdb-only key served — one item
  offering the HD and 4K copies as versions — no longer merges, and a
  cross-library group made by hand via `POST /Videos/MergeVersions` (which
  is itself unscoped, by design) is undone by the next nightly run whenever
  both copies carry a Tmdb id.
