# S19: artist DTO links name real artist items

`DtoService` builds a track's or album's `ArtistItems` and `AlbumArtists`
pairs only for names that match an actual `MusicArtist` item, and leaves the
others out. Ferrofin built a pair for every stored name — from the
`ItemValues` row when no artist item existed — so a performer's link pointed
at an id no `/Items/{id}` request could resolve, and the shape of the lists
(an empty array for an untagged item) differed from Jellyfin's.

Source oracle: Jellyfin `4910aafa1a`, `DtoService.cs:1335-1402`
(`GetArtists` → `LinkedChildrenService.FindArtists`, a clean-name match on
`MusicArtist` rows), `ArtistsValidator.Run` (one artist item per name in
`GetAllArtistNames()`, performers included).

## Behavior

- **Pairs.** `ArtistItems` and `AlbumArtists` contain the distinct, non-blank
  stored names that match a `MusicArtist` item by clean name, each with that
  item's id (the first by sort name when several match). A name with no item is
  omitted — never given an `ItemValues` id. `Artists` stays the full stored list
  and `AlbumArtist` its first name.
- **Shape.** Every kind that carries artist fields always emits the arrays,
  empty when untagged (`[]`, never absent). A music video carries `Artists` and
  `ArtistItems` only (`IHasArtist`); it has no album artists.
- **Items for every artist.** Scans materialised a browsable `MusicArtist` row
  only for album artists, leaving performers as filter-only values — so linking
  strictly to items would have dropped every performer's link. They now
  materialise one `MusicArtist` row per distinct artist name (the existing
  type-and-clean-name guard keeps a name that is both to one row), as
  `ArtistsValidator` does. `/Artists` therefore lists performers, which it could
  not before.
- The lookup is no longer routed through the `ItemValues` rows: it reads
  `MusicArtist` rows by clean name directly, so an item without a stored value
  still links.

## Verification

- Unit: pairs for a track whose stored artists repeat a name and include an
  unknown one (distinct, linked only, with the artist item's id, the plain list
  untouched, `AlbumArtist` the first stored name); an untagged track's empty
  arrays and null album artist; a music video's artists with no album artists;
  the end-to-end clean-name resolution for awkward names now links to the
  `MusicArtist` item rather than an `ItemValues` id; the materialiser makes one
  row for each of a performer, an album artist and a name that is both.
- Production binary, previous baseline vs updated, a FLAC by a guest performer
  on a band's album:

  | | performer's `ArtistItems` id → `GET /Items/{id}` | `/Artists` total |
  |---|---|---|
  | before | **404** (an `ItemValues` id) | 0 |
  | after | **200** | 1 |

  The album artist's link resolved both before and after.

## Limitations

- Jellyfin refreshes each new artist item's metadata in the artists validator;
  Ferrofin's by-name artist pass already refreshes the artists never refreshed,
  so performers now take part in it (extra provider lookups on a scan, as there).
- The `ItemValues`-based fallback ids of earlier databases remain stored; they
  are simply no longer used for these two lists.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,453, extensions 96, livetv 299, wasm
62, server 237. Real FFmpeg tests enabled; the WASM guest build remains
disabled. The first run tripped the SQL boundary ratchet (the artist lookup and
four test updates had put new `sqlx::query` calls in `dto_service.rs`) and a
Clippy type-complexity lint; the lookup moved into `item_repository.rs`, the
tests seed through `test_support` helpers, and the ratchet, Clippy and the full
core suite were rerun clean, while the dependent packages and coverage were
taken before that refactor. Line coverage over core's own sources: **95.13%**,
above the 80% gate; traits and server composition are coverage-exempt. The
instrumented build was cleaned right after the numbers were taken, leaving
`target` at about 24 GiB with about 494 GiB free.
