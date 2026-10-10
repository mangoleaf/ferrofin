# S26: blank and time-formatted chapter names

A video's chapter names are normalized the way Jellyfin normalizes them: a
blank name, or one that .NET's `TimeSpan.TryParse` accepts, is replaced by the
localized `ChapterNameValue` template numbered from 1. Ripping tools often
leave exactly those two forms.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/MediaInfo/FFProbeVideoInfo.cs` `NormalizeChapterNames`
(called after the optional dummy-chapter generation, for every video
refreshed in Default/Full mode).

## Behavior

- `ferrofin_providers::chapter_names::normalize_chapter_names` renames each
  chapter whose name is missing, whitespace-only, or accepted by
  `is_time_span`. The scan applies it to probed and dummy chapters alike; the
  dummy-only naming it replaces is the same rule over a subset.
- `is_time_span` reproduces `TimeSpan.TryParse`'s accept/reject set rather
  than approximating it with a pattern: surrounding Unicode whitespace and one
  leading `-` (no space after it, no `+`); `d`, `h:m`, `h:m:s[.f]`, `d.h:m`,
  `d.h:m:s[.f]`, `d:h:m:s[.f]`; three colon-separated numbers with a first
  number above 23 read as `d:h:m`; an empty seconds field before a fraction
  (`1:02:.5`); hours ≤ 23, minutes and seconds ≤ 59, at most seven fraction
  digits, and a total within signed 64-bit ticks (a negative one tick further).
  A bare number is a day count, so `12` is a time span and `Chapter 01` is not.
  ASCII digits only; full-width and Arabic-Indic digits are rejected, as in
  .NET.

## Verification

- **.NET oracle.** A .NET 10 harness runs `TimeSpan.TryParse` over three
  generated corpora (22,822 + 661 + 148 strings: sign/whitespace/digit-count/
  separator/range/overflow combinations, Unicode whitespace and digits, tick
  limits, the `1:02:.5` family). The Rust function was fitted against it and
  matches every row; a 11,545-row subset (all non-trivial shapes) is checked in
  as `tests/data/time_span_oracle.json` and asserted by a unit test. The
  harness, corpora and complete results are under `/tmp/ferrofin-s26/`.
- Unit test of the rename rule (`None`, empty, whitespace, `00:12:34`,
  fractional time, real names, a bare number, `Chapter 01`).
- Scan regression: probed chapters with the same names persist as
  `Chapitre 1…` under a French UI culture; `Opening` survives.
- Production binary, previous baseline vs updated, on an mkv with eight
  chapters titled (none), empty, blanks, `00:05:00`, `Opening`, `12`,
  `1.02:03:04.5`, `Chapter 01`:

  | | names over HTTP (`/Items/{id}?fields=Chapters`) |
  |---|---|
  | before | `null`, ``, `   `, `00:05:00`, `Opening`, `12`, `1.02:03:04.5`, `Chapter 01` |
  | after | `Chapter 1`, `Chapter 2`, `Chapter 3`, `Chapter 4`, `Opening`, `Chapter 6`, `Chapter 7`, `Chapter 01` |

  This path adds one linear pass over a video's chapters, so no latency
  measurement was taken; the scan's own timings are unchanged in order of
  magnitude.

## Limitations

- The accept set is the invariant/en-US grammar. `TimeSpan.TryParse` uses the
  ambient culture, so a culture with other separators would accept different
  strings; that ambient-culture behavior is the open S33/S34 divergence and is
  not claimed here.
- Only the scan's chapter fold is changed; chapters from other sources go
  through the same fold.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: providers 880 (4 existing skips),
core 2,423, livetv 296, extensions 96, wasm 62, server 230. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first Clippy run flagged
`is_time_span` at 103 lines (`too_many_lines`); it was restructured into a
field-extraction helper and re-verified against the full 23k-row oracle, then
the rerun was clean.

Line coverage over each crate's own sources (nextest, per crate, FFmpeg tests
on): **providers 91.23%**, **core 95.07%**, both above the 80% gate. The new
file alone is at 79.8% of lines; the unreached lines are defensive rejections
of impossible digit groups, and the gate is per crate. The instrumented core
build took `target` to 39.9 GiB, above the 30 GiB cap; coverage caches were
removed after each crate's numbers, leaving 21.5 GiB with about 495 GiB free.
