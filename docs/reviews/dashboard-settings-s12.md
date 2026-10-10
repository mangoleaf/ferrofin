# S12: book local-metadata change monitors

A book's Comic and Open Packaging Format readers now watch their own files,
so editing a sidecar is enough for the next library refresh to re-read it.

Source oracle: Jellyfin `4910aafa1a` — `ComicProvider.HasChanged` (any of the
registered `IComicProvider.HasItemChanged`: `ExternalComicInfoProvider`,
`InternalComicInfoProvider`, `ComicBookInfoProvider`) and
`OpfProvider.HasChanged`, consulted by `MetadataService` as
`IHasItemChangeMonitor` when not every provider is forced to run.

## Behavior

- Each monitor's test is `file.Exists && mtime > item.DateLastSaved` — strictly
  newer, no tolerance (unlike the NFO provider's one-minute allowance).
  Ferrofin's refresh planner gains a `Book` sidecar kind with that rule.
- `books::book_change_mtime(path, readers)` reports the newest mtime among the
  files the *enabled* readers watch; since any watched file being newer is the
  newest being newer, one value is exactly equivalent to upstream's "any
  monitor fired":
  - **Open Packaging Format**: `<stem>.opf`, else `content.opf`, else
    `metadata.opf` beside the book.
  - **Comic Provider**: the external ComicInfo file (`<stem>.xml`, else
    `ComicInfo.xml`) and, for a `.cbz`, the book file itself (the internal
    ComicInfo and ComicBookInfo readers).
  - **EPUB Metadata** has no monitor, and a disabled reader's file is not
    watched.
- The scanner supplies it for Book items (a book has no NFO), using the
  library's own reader selection (`LocalMetadataReaderOrder` /
  `DisabledLocalMetadataReaders`). A changed monitor makes the plan re-run the
  local readers for that item only; an unchanged book stays untouched.

## Verification

- Unit: each reader watches only its own file with upstream's candidate order;
  the specific name beats `content.opf`; EPUB-only and Comic-without-sidecar
  report nothing; `.cbz` is watched itself and `.cbr` is not; the planner's
  `Book` rule at +1 s, +1 ms, equal and older.
- Real server (`dashboard_metadata_locale`): after the reader-order checks, an
  edit to the ComicInfo sidecar alone is picked up by a plain library refresh.
- Production binary, previous baseline vs updated, one comic with a
  ComicInfo sidecar and one book with an OPF sidecar, each edited after the
  first scan and followed by a normal library refresh:

  | | comic title after edit | OPF title after edit |
  |---|---|---|
  | before | `First title` (edit ignored) | `Opf first` (edit ignored) |
  | after | `Edited title` | `Opf edited` |

  The added work is a few `stat` calls per Book item per scan; no timing was
  measured.

## Limitations

- For a book that is a directory, upstream looks for the sidecar inside the
  directory under the directory's name; Ferrofin's existing reader (and so this
  monitor) looks beside it. The reader's behavior is unchanged here.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: providers 896 (4 existing
skips), core 2,428, livetv 296, extensions 96, wasm 62, server 230. Real
FFmpeg tests enabled; the WASM guest build remains disabled. Line coverage
over each crate's own sources: **providers 91.36%**, **core 95.09%**, both
above the 80% gate; traits and server composition are coverage-exempt. The
instrumented core build took `target` to 42.4 GiB, above the 30 GiB cap; each
coverage cache was removed right after its numbers were taken, leaving
23.9 GiB with about 494 GiB free.
