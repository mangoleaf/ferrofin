# Shared music artwork

Music-library scans resolve embedded artwork by the actual MusicAlbum item ID,
including multi-disc descendants. Album titles are not identities: two albums
with the same name do not share an artwork decision.

Existing album/local artwork is preferred. Otherwise the first track with a
usable embedded image supplies the cover. Tracks without a picture or whose
extraction fails do not prevent a later track from supplying it. The scanner
extracts once, computes dimensions/blurhash once, and reuses the result for the
album and tracks. The post-scan pass backfills earlier tracks without pictures.
An album with only a backdrop can still acquire its missing Primary image.

## Storage and edits

Shared covers are immutable files under
`<metadata-library>/album-covers/<album-id>/<sha256>.<extension>`.
The database still gives each track an image reference, so track and album image
URLs remain usable. Existing per-track sidecars and uploads keep precedence;
standalone audio retains its per-item extraction path. Existing private track
covers are preserved rather than guessed to be disposable duplicates.

Uploading a track image writes into its own item directory. Uploading an album
image produces a new shared cover on the next scan, updates shared track
references, and preserves explicit track overrides. Deleting an image removes
its database reference without unlinking a shared cover. Shared files are a
persistent cache and remain on disk even when no references remain: this avoids
breaking concurrent readers/scans or other tracks, including during replacement.
There is no automatic eviction of this shared cache.

No database migration or library-directory writes are required by sharing.
The previous fix that routes ffmpeg temporary extraction through server cache
storage remains in place.

## Validation and measurement

Compared the exact parent `91676c72` with this implementation using separately
built debug servers, fresh disposable databases, and the same 12 one-second FLAC
tracks containing an identical 128×128 JPEG. Remote metadata was disabled. Three
alternating runs per build used `POST /Library/Refresh`, the server's scan elapsed
log field, and then an unchanged rescan. Every album and track Primary image URL
returned HTTP 200 with image data.

| Metric | Parent | Shared artwork |
| --- | --- | --- |
| First scan median (range), ms | 1167 (1153–1171) | 802 (792–811) |
| Unchanged rescan median (range), ms | 313 (311–342) | 320 (320–322) |
| Distinct album/track cover files | 12 | 1 |
| Cover bytes | 3828 | 319 |
| Album/track image references | 13 | 13 |

This small local fixture measures extraction/storage work, not production scan
throughput or remote-provider latency. First scans were about 31% faster;
unchanged rescans were comparable, with a 7 ms higher median in this sample.

The regression test covers multi-disc ownership, early tracks without embedded
pictures, one extraction per album, dimensions/blurhash, rescans, shared-file
survival after deleting a reference, track uploads, and album replacements.
Strict all-targets/all-features Clippy passes for core and providers. Their test
suites pass with 92.99% and 91.40% line coverage respectively.
