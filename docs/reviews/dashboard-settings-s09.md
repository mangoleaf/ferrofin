# S09: book covers are image providers, not a side effect of the metadata read

Jellyfin extracts a book's cover with two dynamic image providers —
`ComicImageProvider` ("Comic Book Archive Cover Extractor", comic archives) and
`EpubImageProvider` ("EPUB Metadata", `.epub`) — each a registered image
fetcher with its own checkbox and place in the library's image order, and each
run by the image pass. Ferrofin extracted the cover inside the **metadata**
reader (`enrich_book`): neither extractor was registered or offered in the
dashboard, the checkboxes and order did nothing, only the Primary limit gated
it, and an image-only refresh never ran it.

Source oracle: Jellyfin `4910aafa1a`, `MediaBrowser.Providers/Books/
ComicImageProvider.cs` (`Name`, `Supports`, `GetSupportedImages`, `GetImage`),
`OpenPackagingFormat/EpubImageProvider.cs`.

## Behavior

- **Registered image fetchers for `Book`.** "Comic Book Archive Cover
  Extractor" is new; "EPUB Metadata" (already the EPUB metadata reader) now also
  carries the image-fetcher capability — upstream's EPUB reader and cover
  extractor share the name, so its checkbox gates both. Both advertise
  `Primary`. They default on, like every built-in fetcher, and take part in the
  saved fetcher list and order (default order 50, as plugins without
  `IHasOrder`).
- **Run in the artwork pass.** For a book, the ordered artwork pass tries each
  ticked extractor in the library's order while the item has no Primary and the
  library's Primary limit is above zero. The comic extractor takes `.cb7`,
  `.cbr`, `.cbt` and `.cbz` files; the EPUB extractor takes `.epub`; each only
  its own type. Ticking one does not extract for the other's files.
- **Not tied to the metadata read.** The cover is no longer produced by
  `enrich_book`; the same code now runs for an image-only refresh (metadata
  mode `None`), a locked item is skipped as other dynamic providers are, and a
  stored extraction is reused unless images are being replaced.
- **Unchanged.** Archive types the port cannot open (`.cbr`, `.cb7`, `.cbt` —
  no maintained pure-Rust reader; see `books::READABLE_ARCHIVE_EXTENSIONS`) are
  recognised as comics but yield no cover or embedded metadata; that is a
  separate, older limitation.

## Verification

- Unit (scan): a `.cbz` is covered with the comic extractor ticked, not with only
  "EPUB Metadata" ticked and not with nothing ticked; the existing Primary-limit
  test (0 / 1) now drives the artwork pass. Registry tests list the two book
  fetchers.
- Production binary, previous baseline vs updated, two comic libraries — one
  with default options, one with every Book image fetcher unticked:

  | | Book image fetchers offered | default library cover | unticked library cover |
  |---|---|---|---|
  | before | none | yes | **yes** (the checkbox did nothing) |
  | after | EPUB Metadata, Comic Book Archive Cover Extractor | yes | **no** |

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on providers passed: providers 917 (4 existing skips), core
2,456, extensions 96, livetv 299, wasm 62, server 237. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first Clippy run flagged
`run_ordered_artwork` at 108 lines; the cover step moved into its own method
and Clippy and the book-cover tests were rerun clean, while the package results
and coverage were taken just before that move. Line coverage over providers'
own sources: **91.54%**, and over core's **95.13%**, both above the
80% gate; traits and server composition are coverage-exempt. Instrumented
builds were cleaned right after their numbers were taken.
