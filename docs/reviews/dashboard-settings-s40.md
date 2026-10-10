# S40: artwork mutation metadata lifecycle

Artwork changes now perform the repository save Jellyfin performs, and
nothing happens when there was nothing to change.

Source oracle: Jellyfin `4910aafa1a` — `BaseItem.DeleteImageAsync`,
`BaseItem.SwapImagesAsync`, `ImageController.PostSetItemImage*`,
`LibraryManager.UpdateItemsAsync`, `ImageInfo.IsLocalFile`.

## Behavior

- **Repository save after a change.** Upload, remote download, delete and
  reorder end with `BaseItem.UpdateToRepositoryAsync(ImageUpdate)`: the metadata
  savers run, the item's `DateLastSaved` is re-stamped (the DTO `Etag` is
  derived from it) and `ItemUpdated` reaches the library-changed notifier.
  New `ProviderManager::update_to_repository` and
  `ItemPersistenceService::touch_item_updated` carry this; the one targeted
  UPDATE avoids rewriting the whole row.
- **Missing slot is "nothing to do".** `delete_image` returns before the
  saver/save when no row was removed. Reorder now reports whether it swapped
  (`swap_item_images`/`swap_images` return `bool`) and the handler saves only
  then.
- **Remote artwork is not swapped** (`!IsLocalFile` on either side), and a
  slot swapped with itself is an ordinary swap that resets dimensions, as
  upstream. Previously the same-index case returned early.
- The remote-download handler no longer calls the saver a second time; the
  provider manager's save path already performs the lifecycle once.

## Verification

- Core: `touch_item_updated` stamps the row, announces exactly one
  `ItemsUpdated` entry and ignores a missing row; swap returns
  `true`/`false` for real, out-of-range, same-slot and remote-path cases.
- Providers: `delete_image` stamps only after a real removal;
  `update_to_repository` stamps through the store and tolerates a manager
  without one.
- API: reorder saves the item only after a real swap.
- Real server (`dashboard_metadata_locale`): uploads move the item's Etag; a
  missing-slot delete leaves Etag and images untouched; a real delete moves it.
- Production binary, previous baseline vs updated, same fixture:

  | Operation | Etag moved before | Etag moved after |
  |---|---|---|
  | upload | no | yes |
  | delete missing slot | no | no |
  | reorder missing slot | no | no |
  | reorder real slot | no | yes |
  | delete real slot | no | yes |

  Median of ten Primary uploads: **7.0 ms before, 9.9 ms after** (an extra
  UPDATE and one row read for the notifier). Sample ranges overlap
  (5.6–16.2 ms after vs 6.2–17.0 ms before) and the host is shared, so this
  is an observation, not a benchmark.

## Limitations

- Reorder still exchanges stored rows rather than swapping the two files on
  disk as upstream does. That is separate from this lifecycle finding and is
  tracked as S41 in the living review.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package that depends on a changed crate passed: traits 76, providers 878
(4 existing skips), core 2,422, api 929, drawing 73, mediaencoding 949,
hls 117, livetv 296, extensions 96, wasm 62, server 230. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first Clippy run failed on
a similar-names lint in a new test; it was renamed and the rerun is clean.
Line coverage over each crate's own sources (nextest, per crate, FFmpeg tests
on): **providers 91.19%**, **core 95.07%**, **api 87.81%**, all above the 80%
gate; traits and server composition are coverage-exempt. The instrumented
builds took `target` to 39.8 GiB (core) and 33.9 GiB (api), above the 30 GiB
cap; each coverage cache was removed right after its numbers were taken,
leaving 21.4 GiB with about 495 GiB free. Worktree, history, production binary,
native datasets and canonical profiles were untouched.
