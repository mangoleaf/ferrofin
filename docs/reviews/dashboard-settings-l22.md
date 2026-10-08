# L22: subtitle destinations and collision handling

Subtitle uploads and provider downloads now read the live owning library's
SaveSubtitlesWithMedia option. Disabled storage selects the item's internal
metadata directory; enabled storage selects the media directory. The selected
write failure propagates before recording a stream. There is no silent retry
in the pinned SubtitleManager. Nested/adopted physical parents resolve through
the shared owning-library helper, and internal paths follow the live metadata root.

Existing subtitle bytes survive repeated writes. Exclusive new-file opens use
numbered collision suffixes. Names lowercase language/format and include forced
then SDH flags. The nine supported formats match NamingOptions; unsupported
formats and path separators in language return invalid-input errors. Image
subtitle codecs stay image codecs; MKS containers await actual probing.

Source oracle: Jellyfin `4910aafa1a`,
MediaBrowser.Providers/Subtitles/SubtitleManager.cs (UploadSubtitle,
TrySaveSubtitle, TrySaveToFiles), Emby.Naming/Common/NamingOptions.cs,
MediaBrowser.Controller/Entities/BaseItem.cs and MediaInfoResolver.cs.

Validation: **56 focused core tests** pass normally and instrumented, including
live destination/root changes, owning-library selection, bytes preserved across
collisions, exclusive concurrent writes, selected-directory failure, formats
and stream codecs. Real HTTP uploads six subtitles across live switches and
checks both files and persisted streams; failed writes/invalid inputs add no
stream. That fixture first refreshes removed L20 sidecars to reconcile its
previous persisted rows. The independent subtitle rescan/probe-failure test,
SQL boundary, formatting, server build and strict workspace Clippy pass.
Core coverage is **94.34%** (104,299/110,562), with no LLVM export warnings;
unchanged coverage is seeded from L21 and affected tests are rerun.

Native before/after/reference runs nine local HTTP scenarios. After-change
statuses, filenames and exact file contents match native Jellyfin **12.1.0**
through off/on/off, collisions, combined forced/SDH, blocked media, unsupported
format and hostile language. Before-change overwrites the same media file,
retries blocked media internally, and accepts invalid formats/languages.
After-change creates six files and retains six streams across all failures.
Jellyfin queues stream probing, so its immediate stream snapshots are not
settled equality evidence; the independent HTTP regression checks persistence.

First media upload is **5.87/3.83 ms** and subsequent media collision
**4.91/5.20 ms**, before/after. The five ordinary successful uploads range
**3.42–6.04/3.83–5.20 ms**. Work differs for collisions/destinations and newly
rejected cases; these unisolated observations make no improvement claim.
Docker is unavailable. The reference uses the existing temporary .NET 10 runtime,
not the host runtime; an initial host-runtime launch failure was replaced.

Evidence: `/tmp/ferrofin-dashboard-l22-{checks,coverage,native}.json`, native
before/after/reference JSON and logs. Builds remain serialized with a 30 GiB
target cap and 512 GiB host reserve; target is approximately 11 GiB.
S18 remains the separately tracked broader scan-matrix fixture repair.
