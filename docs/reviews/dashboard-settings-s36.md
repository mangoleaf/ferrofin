# S36: artwork replacement cleanup failures

Replacing artwork removes the file it replaced, and a failure to do so is no
longer swallowed.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/Manager/ImageSaver.cs` `SaveImage` (the "Delete the
current path" block). Upstream sets the new path on the item, then deletes the
previous local/internal file unless any saved path equals it under
`OrdinalIgnoreCase`; it forgives only `FileNotFoundException`, so any other
failure escapes to the controller *before* its repository save.

## Behavior

- **Failures surface.** `ImageFileSaver::save` now returns the published
  image together with `cleanup` (the failed removal of the replaced file).
  The upload path persists the new image first, then returns the error: the
  client gets a `500`, the replacement stays stored and served, the stuck file
  stays, and the item is not re-saved (no Etag change, no update
  notification), as in upstream where the exception precedes
  `UpdateToRepositoryAsync`. Scans and refreshes (`save_all`) store the images
  and report the failure through their existing failure channel.
- **Only "not found" is forgiven**, in the media-folder path and the
  internal-metadata path alike. The duplicate `warn!` is gone; the error is
  returned and logged once at the outer layer.
- **Case-insensitive saved-path comparison.** A previous path equal to a
  saved path ignoring case is kept (`savedPaths.Contains(…, OrdinalIgnoreCase)`),
  where the old comparison was exact.
- **All image types, not only backdrops.** The earlier internal cleanup was
  specific to backdrops; upstream applies the same rule to every type whose
  file lives under the internal metadata path. It now covers every type, with
  the existing ownership guards kept: the file must be inside this item's
  own internal folder, not the shared album artwork, referenced by one slot,
  and not under a symlinked parent. No arbitrary registered path is deleted.

## Verification

- Unit: `copy_images` reports an undeletable previous file after the new image
  is written and the staged source removed, forgives an absent one, and keeps a
  case-alias; the internal-file cleanup returns its failure and forgives a
  missing file.
- Real server (`dashboard_metadata_locale`): a Primary replacement whose
  previous file is an undeletable directory returns `500`, leaves the
  replacement stored as a file and the stuck file in place. The existing
  destination-fallback check now keeps its Primary in internal storage first,
  because replacing a file that cannot be deleted fails the upload; the case it
  tests (an unwritable *destination*) is unchanged.
- Production binary, previous baseline vs updated:

  | | before | after |
  |---|---|---|
  | replace Primary (png → jpeg) | 204, old file removed | 204, old file removed |
  | replace Primary when the stored file is undeletable | **204** (error hidden) | **500**, new image persisted and served, stuck file kept |

  The normal path is unchanged, so no latency effect is expected; none was
  measured.

## Limitations

- Upstream's case-insensitive path test also applies on case-sensitive file
  systems, as ported; two files differing only in case are therefore treated
  as one.
- A failed cleanup leaves the Etag untouched while the stored image has
  changed, matching upstream's observable state (its in-memory item changes,
  the repository save does not run).

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: providers 882 (4 existing
skips), core 2,423, livetv 296, extensions 96, wasm 62, server 230. Real
FFmpeg tests enabled; the WASM guest build remains disabled.

Line coverage over each crate's own sources (nextest, per crate, FFmpeg tests
on): **providers 91.20%**, **core 95.07%**, both above the 80% gate. The
instrumented core build took `target` to 39.9 GiB, above the 30 GiB cap; each
coverage cache was removed after its numbers were taken, leaving 21.5 GiB with
about 495 GiB free. A first core coverage run overlapped with unrelated
in-progress work and was discarded; the numbers above are from a clean rerun of
exactly the committed tree.
