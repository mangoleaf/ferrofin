# S35: by-name identity settings and long folder names

Two shared identity rules were only half ported: the item-id casing settings,
and the length bound on a by-name entity's folder.

Source oracle: Jellyfin `4910aafa1a` — `LibraryManager.GetNewItemIdInternal`
(`forceCaseInsensitive || !EnableCaseSensitiveItemIds` ⇒ `ToLowerInvariant`),
`LibraryManager.GetItemByNameId` (`forceCaseInsensitive =
EnableNormalizedItemByNameIds`), and `BaseItem.GetItemByNameFolderName`
(128-byte bound; used by `Person`, `Genre`, `MusicGenre`, `Studio`, `Year` and
`MusicArtist` `GetPath`).

## Behavior

- **Id casing follows both settings.** A scanned item's id key is lowercased
  only when `EnableCaseSensitiveItemIds` is false; a by-name id key is
  lowercased when that is false **or** `EnableNormalizedItemByNameIds` is true.
  Ferrofin always lowercased by-name keys and always kept the case of scanned
  ones, so the case-sensitive, non-normalized combination had no
  implementation. The defaults (both true) are unchanged, so no existing id
  moves. The two settings are published to `item_type_lookup` when the
  configuration loads and each time it is saved, so the next derivation sees
  them as upstream's live reads do. Changing them changes the ids derived from
  then on; nothing rewrites ids already stored, which is also upstream's
  behavior.
- **Folder names are bounded.** The sanitized, trimmed name is kept when it
  fits 128 UTF-8 bytes; otherwise it becomes a readable prefix, `-`, and the
  MD5 (`N` format) of the whole sanitized name, cut on UTF-16 units without
  splitting a surrogate pair and with trailing space and dots trimmed before
  the suffix. Every by-name path (`person_path`, `by_name_path`, used for
  people, genres, music genres, studios, years and artists) uses it. Existing
  rows keep the paths they were created with.

## Verification

- **.NET oracle.** The upstream method, copied verbatim and run on .NET 10 over
  39 names (one byte under/at/over the bound; 2- and 3-byte characters at the
  cut; supplementary-plane characters in every alignment of the surrogate
  guard; trailing space/dots exposed by the cut; the credit-list and shared-
  prefix cases of upstream's `BaseItemTests`), matches the Rust output exactly
  and stays within 128 bytes. The upstream xUnit cases are also transliterated.
- A separate test binary (the policy is process-wide) loads the real
  configuration manager, saves each combination of the two settings and checks
  which id keys keep their case — defaults `(true, false)`, case-sensitive but
  not normalized `(true, true)`, not case-sensitive `(false, false)`; a restart
  restores what was saved; and the key rule is checked for all four
  combinations.
- Production binary, previous baseline vs updated: an NFO actor whose name is
  270 characters. The Person folder component was **269 bytes** before (over
  the 255-byte limit of common Linux file systems, the case the upstream
  comment describes) and is **128 bytes** after
  (`Longname … Longn-75790778cd47ec616e1db5eed51b493b`). The id-casing flags are
  not observable over HTTP, because by-name rows are persisted when created and
  ids differ between fixtures only through their temporary data paths; the
  configuration-manager test above is the check for them.

## Limitations

- The policy is a process-wide setting rather than a parameter of
  `IdDerivation`, which 36 call sites build; the configuration manager is its
  only writer.
- Changing the casing settings on an existing library does not migrate stored
  ids, so by-name entities scanned before the change keep their old ids while
  new ones use the new rule (as upstream).

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,436, extensions 96, livetv 296,
wasm 62, server 230. Real FFmpeg tests enabled; the WASM guest build remains
disabled. The first Clippy run flagged `load` growing to 101 lines and a
redundant `to_string`; both were fixed (the call moved into a shared helper),
and Clippy and the touched modules' tests were rerun clean, while the
downstream package results and coverage were taken just before that edit.
Line coverage over core's own sources: **95.10%**, above the 80% gate; traits
and server composition are coverage-exempt. The instrumented build took
`target` to 42.5 GiB, above the 30 GiB cap; its cache was removed right after
the numbers were taken, leaving 23.9 GiB with about 494 GiB free.
