# S41: backdrop reorder exchanges the files

Reordering backdrops swapped the two rows' *paths*. Jellyfin swaps the two
image *files'* contents and leaves each row's path alone, so the files on disk
stay in slot order and the rows keep pointing at their own names.

Found while closing S40; the source is Jellyfin `4910aafa1a`,
`BaseItem.SwapImagesAsync` and `ManagedFileSystem.SwapFiles`
(`ImageController.UpdateItemImageIndex` calls the former).

## Behavior

- `swap_item_images` exchanges the contents of the two files through a
  temporary copy of the first (`SwapFiles`: copy the first aside, copy the
  second over the first, move the copy over the second). A failure part-way
  restores the first file and removes the copy; the rows are not touched.
- Each swapped row keeps its path and has its cached width and height cleared,
  its blur hash cleared (it described the old content; upstream leaves it
  stale) and its modified time re-read from its file, so the image tag moves
  and clients refetch.
- A missing file (the row points nowhere) fails the request, as `File.Copy`
  throws; nothing changes.
- Unchanged: an out-of-range slot or a remote (`http…`) path on either side is
  "nothing to do", and the item is saved afterwards only when files were
  swapped (S40).
- A slot swapped with itself, or two rows naming one file, is now a no-op that
  reports "nothing swapped". Upstream's copy onto the same file throws an
  `IOException` there, which is not worth reproducing; it also supersedes the
  S40 note that such a swap was treated "like any other pair".

## Verification

- Repository: three backdrops swapped 0↔2 trade contents while their rows keep
  their paths, no temporary file remains, the swapped rows lose size and blur
  hash and gain a fresh modified time and the middle one is untouched;
  out-of-range, negative and same-slot requests change nothing; a row whose
  file is gone fails with the other file and both rows unchanged; remote
  paths are ignored. The library manager's swap test checks the same through
  the manager.
- Real server (`dashboard_metadata_locale`): two backdrops with different
  bytes are uploaded and reordered over HTTP; the listed paths are unchanged,
  the files trade contents, and the bytes served at the first index are the
  second image's.
- Production binary, previous baseline vs updated, a red and a blue backdrop
  swapped:

  | | row paths after the swap | file at slot 0 / 1 | served slot 0 / 1 |
  |---|---|---|---|
  | before | exchanged | blue / red | blue / red |
  | after | **unchanged** | blue / red | blue / red |

  What a client sees is the same; what is on disk and in the item's image list
  now matches Jellyfin (a later delete or replace of a slot touches that slot's
  own file).

## Limitations

- Swapping files with different extensions leaves each path's extension
  unchanged, as upstream does.
- The blur hash is cleared where upstream leaves a stale one; this is a
  deliberate correction.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,437, extensions 96, livetv 296,
wasm 62, server 230. Real FFmpeg tests enabled; the WASM guest build remains
disabled. The trait change is documentation only. The first Clippy run flagged
two style lints in the new code (`or_else` returning `Err`, a
`map().unwrap_or_else()`); they were fixed and Clippy rerun clean with the swap
tests, while the downstream package results and coverage were taken just
before. Line coverage over core's own sources: **95.10%**, above the 80% gate;
traits and server composition are coverage-exempt. The instrumented build took
`target` to 42.6 GiB, above the 30 GiB cap; its cache was removed right after
the numbers were taken, leaving 23.9 GiB with about 494 GiB free.
