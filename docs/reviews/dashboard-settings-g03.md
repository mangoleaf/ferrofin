# G03: apply the dashboard cache directory

Saved `CachePath` now overrides the startup cache directory when loading the
configuration and when replacing an override. Existing services resolve the
current directory at the start of an operation: resized images, OMDb records,
similarity responses, Schedules Direct country files, frame-extraction scratch
files, Live TV buffers, and extension fingerprints/scratch files. Path-manager,
playback-planner, storage and maintenance consumers already read shared paths.
An operation retains its chosen destination while it runs; existing files are
not moved or removed.

This follows Jellyfin v12.0-rc7 (`4910aafa1a`),
`BaseConfigurationManager.UpdateCachePath` / `ValidateCachePath` and
`BaseApplicationPaths.CreateAndCheckMarker` / `CreateCacheDirTag`.
Changed nonblank paths must exist and accept a temporary write. Conflicting
`.jellyfin-*` markers are rejected and successful roots receive `.jellyfin-cache`
and the standard `CACHEDIR.TAG` signature. Validation and preparation happen
before configuration persistence/publication, so failures preserve saved and live
settings.

Upstream retains the live cache root when an override is cleared; restarting then
restores the startup CLI/environment/default root. Ferrofin preserves this rule
and additionally sets `HasPendingRestart` when clearing requires a restart.

## Verification

All 208 focused tests pass across configuration/path management, image processing,
provider caches, frame extraction, Live TV and extensions. Regressions cover live
cache replacement, persisted overrides, clearing/reloading, rejected paths,
backup tags, real image writes and a live OMDb cache switch. Formatting, strict
workspace Clippy and the server build pass.

Real HTTP checks on the same disposable scanned-video fixture confirm:

- Both successive cache overrides change `/System/Info.CachePath` immediately;
  the next resized profile-image request creates a file beneath the new root.
  Before the fix both overrides were ignored and neither root received a file.
- Missing paths return **404**; read-only paths and conflicting directory markers
  return **500**. All three preserve the previous `system.json` bytes. Previously
  all returned **204** and replaced the saved configuration.
- Clearing retains the second override and sets `HasPendingRestart=true`.

Cached resized-image GETs used 10 curl warmups and 50 measured requests. Median
latency was **2.068 → 1.502 ms**, p95 **8.574 → 3.228 ms**. These are noisy local
measurements, not an improvement claim. The batch's final validation will include
workspace tests, doctests, per-crate coverage and the browse benchmark for the
combined changes to request paths.
