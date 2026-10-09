# L16: automatic NFO saver selection

The selected Nfo saver now runs after persisted metadata edits, automatic and
explicit refreshes, music refreshes, and artwork changes. The composition root
attaches the saver to the provider manager; the scanner invokes it only after
persisting the item's metadata, IDs, people, streams and images. Saver failures
are logged without reversing an otherwise successful edit.

An explicit `MetadataSavers` list takes precedence over legacy and server
settings. Names match without regard to case; an empty list disables writes.
With no list, server `DisabledMetadataSavers` applies, then `SaveLocalMetadata`.
An edit can update an existing sidecar even when the legacy flag is off.
Metadata imports normally do not rewrite NFOs; existing season sidecars retain
upstream's exception. Artwork updates write only when saving image paths is
enabled, apart from that season exception. Force-save and replace-all refreshes
also invoke selected savers when no remote provider answers.

The filesystem writer chooses the upstream destination for movies, mixed-folder
videos, episodes, series, seasons, albums and artists. Unsupported kinds,
channel items and video extras do not generate NFOs. Writes are serialized per
path, compare the existing bytes, and use atomic replacement. The serializer
includes persisted metadata and IDs, field locks, credits, music children,
stream details, artwork paths and the configured user's export data. Unknown
nested custom tags survive; stale owned tags are replaced or removed. Artwork
uses the server's path-substitution mappings as the pinned saver does.

Sources: Jellyfin `4910aafa1a`, ProviderManager.SaveMetadataAsync /
IsSaverEnabledForItem, MetadataService.SaveInternal, BaseNfoSaver and the
Movie/Episode/Series/Season/Album/Artist savers. The disposable native reference
run is Jellyfin **12.1.0**; it corroborates the seven edit-selection scenarios
without changing the source pin.

The separate NFO dashboard findings remain independently open: L16 wires the
export machinery but does not complete selected-user import or validate the
entire NFO settings page. L17 covers artwork file destinations.

Validation passes: **845 provider tests**, **488 focused core tests**, **893 API
tests**, real HTTP, formatting, SQL boundary, server build and strict workspace
Clippy. The HTTP regression covers automatic refresh, case-insensitive selection,
empty selection, existing-file fallback, immediate configuration changes,
custom-tag preservation, image-path suppression and an unwritable sidecar.
Unit regressions also cover per-kind destinations, update thresholds, inherited
XML namespaces, media stream fields, actor thumbnails and user export data.

Separate line coverage passes: providers **92.54%** (23,201/25,070), core
**94.69%** (103,668/109,481), API **86.12%** (40,146/46,616). Core coverage combines
the L15 profile with current focused tests and 22 current binaries; this is
accumulated coverage, and LLVM reports function-data mismatches. The final
provider/API runs are fresh and all instrumented tests pass. Initial API failures
were the old item-update test double's unimplemented saver; that fixture now
accepts and checks metadata-edit saves.

All seven native edit-selection/custom-tag outcomes match Jellyfin. Comparable
before/after edit timings wait for the initial scan to become idle. The medians
and ranges below measure database edits without/with the new filesystem work,
not an isolated benchmark or a performance improvement. Docker was unavailable.

Raw records: `/tmp/ferrofin-dashboard-l16-{checks,coverage,native}.json`, the
`*-settled.log` fixture logs and the final HTTP regression log.


Native edit median: **4.21 ms before** (3.27–6.20), **9.74 ms after** (4.37–16.11), seven edits each. Runs were unisolated and overlapped validation.
