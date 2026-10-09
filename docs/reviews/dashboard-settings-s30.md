# S30: localization dropdown resource catalog

`GET /Localization/Options` now exposes all 105 bundled core translations,
including `lt-LT` and the 35 resources missing from the previous list. Values
come from the resource catalog; labels use .NET native culture names, with
explicit `English` for `en-US`, and sort with ordinal-ignore-case semantics.
The old web labels and 71-entry ordering no longer hide supported languages.

The source oracle is Jellyfin `4910aafa1a`,
`Emby.Server.Implementations/Localization/LocalizationManager.cs`,
`BuildLocalizationData` and `GetDisplayName`. The vendored label snapshot
was generated with .NET 10.0.401 on the resource codes already bundled here:
replace underscores with hyphens for CultureInfo lookup, use NativeName,
fall back to the code for an unknown culture, add English and sort. The
complete snapshot equals the local Jellyfin 12.1.0 HTTP response, including
novelty `pr`, underscore cultures, Lithuanian and non-Latin label ordering.
Regenerate the labels when updating the pinned core resource catalog; the
unit regression requires exactly one option per resource and checks the full
native order. This dropdown fix does not close S33's separate ambient-culture
lookup finding.

The disposable HTTP fixture reproduced 71 options and `lt` on the previous
production binary. The reference and updated production binary must return
the complete 105-option snapshot. Ten requests per binary retain response
bodies, hashes and elapsed times under `/tmp/ferrofin-followup-two/s30/`.
Shared-host timings are observations, not publishable benchmark results.
The first targeted run caught an incorrect test fingerprint (`af` first);
the captured full catalog correctly starts with `ab`. Its corrected rerun
and the original failure remain in the evidence directory.

Final shared validation is recorded with S16 after this two-finding batch.

The corrected targeted run passed 75 tests, and the production build and
three native phases passed. Median request elapsed time across ten local
requests: previous Ferrofin 0.791 ms, updated Ferrofin
0.657 ms, reference Jellyfin 6.928 ms.
These samples include HTTP overhead and are not isolated performance claims.
