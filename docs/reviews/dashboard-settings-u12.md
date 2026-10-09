# U12: download permission and file eligibility

Download authorization already matched Jellyfin: ordinary users with
`EnableContentDownloading=false` receive 403; administrators pass the route
policy but fail the per-item check with 400. Saved off/on/off changes apply
immediately to GET, HEAD and the user-facing `CanDownload` field.

Fixed two additional defects found during verification:

- `CanDownload` and the endpoint now share the upstream item-kind/file-protocol
  predicate. DVD/BluRay videos, remote video/audio/book sources, folders,
  playlists and unsupported leaf types cannot be downloaded. ISO video remains
  eligible; Photo retains Jellyfin's unconditional file-eligibility override.
- Downloads serve the requested item's own path. Resolving playback sources
  previously promoted an alternate version to its primary and returned the
  wrong file.

Compared with Jellyfin `4910aafa1a` (`v12.0-rc7`), `LibraryController.GetDownload`
and `BaseItem`/`Video`/`Audio`/`AudioBook`/`Book`/`Photo.CanDownload`. The new DTO
service seam adds no repository query in production. The shared user-scoped item
visibility issue remains U19.

**89 DTO tests and eight streaming tests** pass, including single/page DTO and
endpoint eligibility agreement, saved permission changes, download headers and
alternate-version bytes. Formatting, strict workspace Clippy, the SQL boundary
gate and the server build pass.

A disposable server verified 12 ordinary/admin off/on/off ranged GET/HEAD
requests, including exact bytes and attachment headers. Before/after fixture
rows reproduced both defects: DVD changed from `CanDownload=true`/206 to
false/400, and the alternate changed from the primary's bytes to its own bytes.
Fifty warm HEAD requests after ten warmups measured median **1.246 → 0.686 ms**,
p95 **2.075 → 0.758 ms** on the shared host; no performance claim is made.

The preceding U08–U11 workspace checkpoint ran 7,779 tests: 7,772 passed and
seven watcher-dependent tests failed, with five skipped. A minimal inotify watch
also fails with ENOSPC: the host's watch quota is exhausted. This is not a green
workspace gate. At that checkpoint separate line coverage was API **87.47%**
and core **92.30%**; U12's new core/API code requires the next coverage run.
