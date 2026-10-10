# S15: subtitle provider namespaces

Remote subtitle candidate ids and provider descriptor ids now use Jellyfin's
namespace: the .NET `name.ToLowerInvariant().GetMD5().ToString("N")` of the
provider name (`opensubtitles` → `29c0c3633fefe8ab4d5b9af1b0275f3d`).

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/Subtitles/SubtitleManager.cs`
(`Normalize`, `GetProviderId`, `GetProvider`, `GetRemoteSubtitles`,
`GetSupportedProviders`).

## Behavior

- Providers return their own local ids. The manager prefixes each search
  candidate with `{md5}_` using the candidate's `ProviderName` (falling back to
  the provider's name), exactly as `Normalize` does. The OpenSubtitles adapter
  no longer pre-prefixes its ids with a readable name.
- `GetSupportedProviders` advertises the same MD5 id.
- Download and raw-content routing split on the first `_` only
  (`Split('_', 2)`), so later underscores stay in the local id; an id with no
  underscore is both provider id and local id (`parts[0]`/`parts[^1]`). The
  readable-name prefix is no longer an alias and is rejected.
- Provider filters and fetcher order remain name-based, as upstream
  (`DisabledSubtitleFetchers`, `SubtitleFetcherOrder` compare `Name`).

The hash reuses the existing `ferrofin_common::extensions::get_md5` (.NET
UTF-16LE MD5 wrapped in a `Guid`); no new dependency or SQL.

## Verification

- Core regressions: known hash for `opensubtitles` computed independently
  from the .NET Guid layout, case-insensitive naming, descriptor id, search→
  download round trip, readable-name rejection, later-underscore and
  no-underscore routing. Existing provider-order/fallback/filter tests now
  assert namespaced ids.
- Real server integration (`dashboard_metadata_locale`): a manual search over
  the real router, manager and OpenSubtitles adapter returns an id with the
  MD5 namespace; `/Providers/Subtitles/Subtitles/{id}` serves it (200) and the
  old `OpenSubtitles_{local}` form is 400. The automatic-download path in the
  same test exercises the namespaced id end to end.
- Production binary (previous baseline vs updated), ten manual searches on a
  scanned movie, median **2.53 ms → 2.42 ms**. Limitation: the production
  binary has no OpenSubtitles endpoint override and no credentials in a clean
  data directory, so these samples measure the item/search path with an
  unconfigured provider and return no candidates; hashed/readable/bare
  download ids all return 400 there for that reason. Namespacing itself is
  verified by the integration test above. Shared-host timings are
  observations, not benchmarks.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: providers 876 (4 existing
skips), core 2,420, livetv 296, extensions 96, wasm 62, server 230. Real
FFmpeg tests enabled; the WASM guest build remains disabled. Independent
packages (api, db, model, traits, hls, mediaencoding, …) do not depend on the
changed crates and keep their previous passing results. Separate line
coverage over each crate's own sources (nextest, per-crate, FFmpeg tests on):
**providers 91.16%**, **core 95.07%**, both above the 80% gate. Traits and
server composition are coverage-exempt. The instrumented build briefly took
`target` to 39.5 GiB, above the 30 GiB cap; it was removed immediately after
the numbers were taken (coverage cache only), leaving 21.0 GiB with about
510 GiB free. Worktree, history, production binary, native datasets and
canonical profiles were untouched.
