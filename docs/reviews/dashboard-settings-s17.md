# S17: embedded tags become an audio item's credits

`AudioFileProber.FetchDataFromTags` does more than copy artist names onto the
item: it replaces the item's **people** with the tags' album artists,
performers and composers (and, for an audiobook, authors, narrators and cast),
so the cast/crew list, `/Persons`, and the per-person browse pages know them.
Ferrofin applied the normalised `Artists`/`AlbumArtists` columns but never
wrote the credits, so a scanned track had none.

Source oracle: Jellyfin `4910aafa1a`, `AudioFileProber.FetchDataFromTags`
(`:174-345`), `PeopleHelper.AddPerson` and `ILibraryManager.UpdatePeople`.

## Behavior

- **A music track** is credited, in this order, with its album artists
  (`AlbumArtist`), performers (`Artist`) and composers (`Composer`). Names are
  the library's own interpretation of the tags — `ALBUMARTISTS`/`ARTISTS` when
  the library prefers the nonstandard tags, custom delimiters and the
  whitelist applied — and the album artists are **not** replaced by the
  performers here (that fallback is only for the stored `AlbumArtists` column).
  `COMPOSER` splits on the tag separator only.
- **An audiobook** is credited with its authors (the album artists, else the
  performers; `Author`), the composers as `Narrator`, and each performer who is
  not an author as `Actor` (cast).
- Blank names are dropped; `update_people` trims and de-duplicates by name and
  type like `AddPerson`.
- The list is written whenever an unlocked audio item is probed, **replacing**
  the stored credits — a track that names nobody clears them, as
  `UpdatePeople(audio, people)` with an empty list does — unless the item's
  `Cast` field is locked. A provider answer that carries credits later in the
  same pass replaces the tags' in turn, as the provider merge does upstream.

## Verification

- Unit: the credit list for a track (order, blanks, the composer split, no
  album-artist fallback, no tags) and for an audiobook (authors from album
  artists else performers, narrators, cast without the authors).
- Scan (`library_scan_audio_credits`): a track's tags become four credits; after
  the file is "edited" and re-tagged, the next scan probes it again and
  *replaces* them; with `Cast` locked, a re-tag leaves the stored credits.
- Production binary, previous baseline vs updated, a FLAC with
  `ARTIST=Lead`, `ALBUM_ARTIST=The Band`, `COMPOSER=Bach`:

  | | `People` of the scanned track |
  |---|---|
  | before | none |
  | after | **Bach (Composer), Lead (Artist), The Band (AlbumArtist)** |

## Limitations

- ATL's multi-value joining (the internal separator) is what the ffprobe tag
  path produces for repeated tags; a single ffprobe string such as `A;B` is one
  name unless the library enables custom delimiters, as for the stored artists.
- Each credited name also becomes a browsable `Person` row, as it does for any
  other credit.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,452, extensions 96, livetv 299, wasm
62, server 237. Real FFmpeg tests enabled; the WASM guest build remains
disabled. Line coverage over core's own sources: **95.12%**, above the 80%
gate; traits and server composition are coverage-exempt. The instrumented
build was cleaned right after the numbers were taken, leaving `target` at
about 24 GiB with about 494 GiB free.
