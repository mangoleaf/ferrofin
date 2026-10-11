# S28: generic NFO ordering follows the culture, and duplicates fold Unicode

`BaseNfoSaver` sorts most of its string output with `OrderBy(x => x)`, which
uses `Comparer<string>.Default` — the actual `CurrentCulture` — and drops
duplicate directors and writers with `StringComparer.OrdinalIgnoreCase`.
Ferrofin used Rust's byte-wise `cmp` for the ordering and ASCII-only equality
for the duplicates, so non-ASCII names were ordered differently from the same
item saved by Jellyfin and `Élodie`/`élodie` stayed two directors.

Source oracle: Jellyfin `4910aafa1a`, `BaseNfoSaver` (`:508-534`, `:537`,
`:680-705`, `:755-769`, `:929-932`), `MovieNfoSaver` and `AlbumNfoSaver`
artists, `GetTagForProviderKey`; review note
`/tmp/ferrofin-n02-independent-culture-review/S28-generic-nfo-ordering.md`.

## Behavior

Every generic ordering now uses the culture collation of the current culture
(the same comparer N02 uses for backdrop paths), stable like `OrderBy`:

- actors, by `SortOrder ?? 0` and then the trimmed name;
- directors, and writers/credits, trimmed, de-duplicated, then ordered;
- trailers, ordered by the trimmed original URL before the output rewrite;
- countries, genres, studios and tags (or styles for music kinds);
- music-video and album artists and album artists;
- the remaining custom provider-id tags, by provider key.

Also:

- Director/writer de-duplication is `OrdinalIgnoreCase`: characters are
  upper-cased one-to-one (`é`/`É`, `ж`/`Ж` equal; `ß` and `SS` not, as .NET's
  `ToUpperInvariant`; a precomposed `é` and `e`+combining accent are not).
- The custom provider tag is `ToLowerInvariant()` of the key, so a non-ASCII key
  (`Äpple` → `äppleid`) is lower-cased like any other letter; the ASCII-only
  rule left `<Äppleid>`.
- The comparer is built once per culture per thread; an unusable culture falls
  back to byte order instead of failing the save.

## Verification

- Unit: a movie serialized under `en-US` and under `sv` puts genres, studios,
  tags, countries, directors, trailers, custom provider tags and actors in
  opposite relative order for `Ä…` versus `Z…` (English sorts `Ä` beside `A`,
  Swedish after `Z`); the `OrdinalIgnoreCase` equalities and non-equalities and
  de-duplication keeping the first spelling; the provider-tag lower-casing.
- Production binary, previous baseline vs updated, a movie whose genres are set
  to `Zebra`, `Ärlig`, `Apa` by `POST /Items/{id}` under two `Accept-Language`s,
  read back from the saved `movie.nfo`:

  | request culture | before | after |
  |---|---|---|
  | `en-US` | Apa, Zebra, Ärlig | Apa, **Ärlig, Zebra** |
  | `sv` | Apa, Zebra, Ärlig | Apa, Zebra, Ärlig |

## Limitations

- A box set's `<collectionitem>` list (`BaseNfoSaver.cs:808-825`) is part of the
  container-folder work (S11) and is not written yet.
- Comparisons use the collation tables bundled with the build, as the rest of
  the culture seam does; a culture .NET knows and the tables do not falls back
  to byte order.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on providers passed: providers 917 (4 existing skips), core
2,448, extensions 96, livetv 299, wasm 62, server 237. Real FFmpeg tests
enabled; the WASM guest build remains disabled. Line coverage over providers'
own sources: **91.54%**, above the 80% gate; only providers changed, and
traits and server composition are coverage-exempt. The instrumented build was
cleaned right after the numbers were taken, leaving `target` at about 24 GiB
with about 494 GiB free.
