# S13: custom NFO release-date formats

`XbmcMetadataOptions.ReleaseDateFormat` is any .NET date pattern. Ferrofin
accepted the setting but wrote every date as `yyyy-MM-dd`, read only that one
shape, and its scanner never consulted the saved options at all.

Source oracle: Jellyfin `4910aafa1a` —
`BaseNfoSaver.AddCommonNodes` / `EpisodeNfoSaver` / `ArtistNfoSaver`
(`DateTime.ToString(format, InvariantCulture)`) and
`XmlReaderExtensions.TryReadDateTimeExact` (`DateTime.TryParseExact(…,
InvariantCulture, AssumeUniversal | AdjustToUniversal)`), used by
`BaseNfoParser` for `aired`/`formed`/`premiered`/`releasedate`/`enddate`.

## Behavior

- **`xbmc::dotnet_date`** reproduces both halves for UTC values: standard
  single-letter formats (`d D f F g G m M o O r R s t T u U y Y`), the empty
  format (`G`), custom patterns (`d f F g h H K m M s t y z`, `:` `/`, quoting
  with `'`/`"`, `\` and `%`), the invariant-culture month/day names, and — for
  reading — field widths, name matching ignoring case, `h`/`H` with `tt`
  consistency, 12-hour handling, `z`/`zz`/`zzz`/`K` offsets (±14:00),
  duplicate fields that must agree, weekday validation, the two-digit-year
  pivot (2049), year range 1–9999, and the defaults for fields a pattern
  omits (today's date; month and day 1 once any date field is given).
- **Writing.** Every release date the NFO savers emit goes through it. A
  malformed pattern throws `FormatException` upstream when a date is written;
  `serialize` now refuses the save in that case (`release_date_format_error`),
  and only when a date would actually be written. Dates aren't touched by the
  separate `dateadded` timestamp.
- **Reading.** `TryReadDateTimeExact` matches the element text as written —
  no trimming, no lenient fallback — so ` 1999-03-31 `, `1999-3-31` and
  `1999-03-31 00:00:00` are not dates under the default pattern, as upstream.
- **The scanner reads the saved options.** `fetch_local_nfo` used
  `NfoConfiguration::default()` regardless of the setting. It now reads
  `named/xbmcmetadata.json` (`ReleaseDateFormat`, `UserId`) per sidecar through
  `NfoConfigurationFile`, which stat-checks the file and re-parses only when it
  changed, so a dashboard edit applies to the next item without a restart.

## Verification

- **.NET oracle.** A .NET 10 harness produced 1,947 formatting rows (183
  formats × 11 instants, including quoting/escape/exception cases) and 25,166
  parse rows (13,768 + 11,398 generated texts: formatted values, case,
  padding, truncation, offsets, designators, duplicates, weekday mismatches,
  range edges) with `TryParseExact`'s result, including the
  `FormatException` it throws for a lone non-standard character. The Rust
  output matches every row; a 5,214-row subset (all accepted rows and every
  exception) and all formatting rows are checked in under
  `crates/ferrofin-providers/tests/data/` and asserted by unit tests. The
  parse oracle ran on 2026-10-10, the date undetermined fields resolve to, so
  the test fixes that date. `z`/`zz`/`zzz` format the host's local offset
  (as .NET does for a UTC value) and have their own host-independent test.
  Harness, corpora and complete results are under `/tmp/ferrofin-s13/`.
- NFO level (`tests/xbmc_release_date_format.rs`): `dd.MM.yyyy`, `yyyy/M/d`,
  `MMMM d, yyyy`, `yyyyMMdd` written for movie, episode (`aired`) and artist
  (`formed`/`disbanded`) and read back; the default pattern rejects what a
  custom one wrote; padded/short/long dates are rejected; a malformed pattern
  is refused only when a date would be written.
- Scan (`library_scan_extras`): sidecars with `05.12.2010` /
  `December 5, 2010` are dated when the pattern says so and undated when it
  doesn't.
- Real server (`dashboard_metadata_locale`): changing the saved
  `ReleaseDateFormat` rewrites the NFO's `<premiered>` as `31.03.1999` /
  `March 31, 1999`, and an invalid pattern fails the NFO save without losing
  the edit.
- Production binary, previous baseline vs updated:

  | | before | after |
  |---|---|---|
  | `<premiered>` written with `dd.MM.yyyy` | `1999-03-31` | `31.03.1999` |
  | with `MMMM d, yyyy` | `1999-03-31` | `March 31, 1999` |
  | refresh of an NFO with `<premiered>05.12.2010</premiered>` under `dd.MM.yyyy` | no date | `2010-12-05` |
  | scan of 3,000 movies with NFO sidecars (median of 3) | 14.54 s | 14.74 s |

  The scan difference (+1.4 %) is within the run-to-run spread (14.5–15.0 s)
  on a shared host: one `stat` per sidecar. Not a benchmark.

## Limitations

- A malformed pattern while *reading* yields no date for that element;
  upstream's `TryParseExact` would throw out of the NFO read for a lone
  non-standard character. Writing is faithful (the save is refused).
- Local-time zones for `z`/`zz`/`zzz` follow the host's time-zone database,
  as .NET's do; historical offsets for extreme years can differ between the
  two databases.
- Only UTC values are formatted, which is what Jellyfin stores.
- Live TV recording NFOs (`RecordingsMetadataManager`) also use
  `ReleaseDateFormat` upstream. Ferrofin writes no recording NFO at all (no
  `.nfo` code in `ferrofin-livetv`); that gap is DVR04 in the living review,
  not part of this finding, and will use `dotnet_date` when it is ported.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: providers 890 (4 existing
skips), core 2,428, livetv 296, extensions 96, wasm 62, server 230. Real
FFmpeg tests enabled; the WASM guest build remains disabled. The first Clippy
run flagged the new module (`manual_is_multiple_of`, `format_push_string`, one
test-only `unnecessary_lazy_evaluations`); those style fixes touch only
padding helpers, were re-verified against the oracle, and the full Clippy and
the providers suite were rerun clean afterwards, while the downstream package
runs and coverage below were taken just before them.

Line coverage over each crate's own sources (nextest, per crate, FFmpeg tests
on): **providers 91.33%**, **core 95.09%**, both above the 80% gate; traits and
server composition are coverage-exempt. The instrumented core build took
`target` to 42.4 GiB, above the 30 GiB cap; each coverage cache was removed
right after its numbers were taken, leaving 23.9 GiB with about 494 GiB free.
