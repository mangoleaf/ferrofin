# G04: apply metadata directory changes consistently

`MetadataPath` now reaches existing scanner artwork and external-stream readers,
dynamic images, user-profile uploads, item-image uploads, lyrics, subtitle
fallback uploads and user-view creation through the live application path.
Existing path-manager consumers already resolve the current root. Changing a root
controls subsequent operations; it does not relocate previously saved files.

Path validation now follows Jellyfin v12.0-rc7 (`4910aafa1a`),
`ServerConfigurationManager.ValidateMetadataPath` / `UpdateMetadataPath`:
changed nonblank directories must exist and accept a temporary write, while
startup/default/unchanged paths are created if absent. Missing changed paths
return 404; write failures return 500. Preparation occurs before configuration
persistence and publication. Clearing resets the metadata directory immediately
to its default, unlike clearing the cache override in G03.

The image-deletion guard also recognizes shared album artwork in an earlier
metadata root. A root change must not make deleting one track's image delete an
asset still referenced by other tracks.

## Verification

All 394 focused configuration, metadata-service and scanner tests pass. Changed
regressions exercise a live profile-image directory switch, subtitle fallback
writes, lyric downloads/readback, read-only directory rejection, clearing to the
default, and retaining shared album art after changing roots. Formatting, strict
workspace Clippy and the server build pass.

The same disposable scanned-video fixture was exercised over real HTTP on both
builds. Before the fix, two successive metadata overrides changed system
information but received **zero** profile-image, item-image or subtitle files.
After the fix, each received all three uploads immediately. Clearing sent the
next uploads to the default root. A missing path now returns **404** instead of
400; a read-only directory returns **500** instead of a successful 204. Both
rejections preserve the previous configuration file.

Saving an identical server-configuration document, on the same fixture, used 10
curl warmups and 50 measured POSTs per build. Median latency was **3.447 →
1.260 ms**, p95 **11.163 → 2.331 ms**. Shared-host noise is substantial; these
numbers are not an improvement claim. Playback-info timings taken during the
upload probes are not used for comparison because each probe added subtitle
streams. Final workspace tests, doctests, coverage and the combined browse
benchmark remain batch-end gates.
