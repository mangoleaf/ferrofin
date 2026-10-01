# Shared provider keys

Audit requested during the investigation of [issue #23](https://github.com/mangoleaf/ferrofin/issues/23).
Work is on `fix/movie-metadata-23`, based on scan-change-detection commit `71311ef2`.

## Findings and changes

Jellyfin distributes application keys in its source. Users do not need to register
individual keys for the metadata providers below. Ferrofin already included four
of the six shared keys found across its overlapping metadata, artwork and subtitle
providers. This change adds the missing OMDb and OpenSubtitles defaults.

| Provider | Upstream credential source | Ferrofin result |
|---|---|---|
| TMDB | [`TmdbUtils.ApiKey`](https://github.com/jellyfin/jellyfin/blob/master/MediaBrowser.Providers/Plugins/Tmdb/TmdbUtils.cs) | Already present. The `TmdbApiKey` plugin setting overrides it. |
| OMDb | [`OmdbProvider.GetOmdbUrl`](https://github.com/jellyfin/jellyfin/blob/v10.11.8/MediaBrowser.Providers/Plugins/Omdb/OmdbProvider.cs) | Added. Unset/blank `FERROFIN_OMDB_KEY` or `omdb_api_key` uses the shared key; a nonblank value overrides it. |
| TVDB | [`PluginConfiguration.ProjectApiKey`](https://github.com/jellyfin/jellyfin-plugin-tvdb/blob/master/Jellyfin.Plugin.Tvdb/Configuration/PluginConfiguration.cs) | Already present. `FERROFIN_TVDB_KEY` overrides it; `FERROFIN_TVDB_PIN` supplies a subscriber PIN. |
| fanart.tv | [`Plugin.ApiKey`](https://github.com/jellyfin/jellyfin-plugin-fanart/blob/master/Jellyfin.Plugin.Fanart/Plugin.cs) | Already present. `FERROFIN_FANART_KEY` supplies an optional personal `client_key` alongside the project key, matching the provider's API. |
| TheAudioDB | [`AudioDbArtistProvider.ApiKey`](https://github.com/jellyfin/jellyfin/blob/master/MediaBrowser.Providers/Plugins/AudioDb/AudioDbArtistProvider.cs) | Already present in the API URL. |
| OpenSubtitles | [`OpenSubtitlesPlugin.ApiKey`](https://github.com/jellyfin/jellyfin-plugin-opensubtitles/blob/master/Jellyfin.Plugin.OpenSubtitles/OpenSubtitlesPlugin.cs) | Added. An account with no `ApiKey` uses the shared application key; a nonblank `ApiKey` overrides it. Downloads still need account credentials. |

The OMDb key was checked against both Jellyfin v10.11.8 and current upstream. The
other constants were compared with the local Jellyfin/plugin checkouts and current
upstream source. One live OMDb lookup using the shared key returned HTTP 200,
`Response=True` and the expected movie (Inception). Other providers were audited
against source; this does not establish their current service availability or quotas.

MusicBrainz metadata lookups, ListenBrainz Labs similarity queries,
[LRCLIB lyric lookups](https://github.com/jellyfin/jellyfin-plugin-lrclib/blob/master/Jellyfin.Plugin.LrcLib/LrcLibProvider.cs)
and the studio-image repository use public read endpoints without API keys.
Local NFO/XML readers, local artwork, embedded book/photo/media metadata and the
local similarity scorer do not contact credentialed services. No shared keys are
missing in those paths.

## Behavior

OMDb is now usable with the library's existing metadata/image fetcher settings and
no extra environment variable. A blank override selects the built-in key; disabling
the provider is done through its library checkboxes. Explicit operator keys are
trimmed and retained. This change can add requests for libraries where OMDb was
selected but previously inactive because the server had no key configured.

OpenSubtitles accepts the same username/password-only configuration as Jellyfin.
Login, search and download requests all select the effective application key.
Completely unconfigured installs remain inactive; an explicit key alone continues
to permit searches as before. The shared application key does not grant an account
or remove provider-side download limits.

The scan verifier now treats OMDb as enabled when its metadata checkbox is selected.
The configuration, feature and upgrade docs describe the new defaults.

## Validation

- Provider tests cover omitted/blank OMDb keys, explicit overrides and actual HTTP
  query parameters. OpenSubtitles tests cover account-only settings, incomplete
  credentials, overrides, and HTTP login requests carrying the selected key.
- The real-server HTTP scan matrix boots with an empty operator OMDb key and a
  local mock endpoint. An OMDb-only movie library receives an overview, IMDb ID and
  Rotten Tomatoes score. Disabling its checkbox stops OMDb requests. The existing
  matrix also verifies no OMDb requests occur in libraries that select only TMDB.
- Provider mocks isolate the tests from real provider accounts and their quotas.

Completed checks:

- `cargo nextest run -p ferrofin-providers`: 578 passed, 4 skipped.
- `cargo test -p ferrofin-server --test scan_change_detection -- --nocapture`:
  the complete HTTP scan matrix passed, including the issue #23 outage cases.
- `bats verify/tests/verify.bats`: 33 passed.
- `cargo clippy -p ferrofin-providers -p ferrofin-server --all-targets --all-features -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Rust checks used this worktree's isolated target directory with `RUSTC_WRAPPER=`.
The full workspace test suite was not run.
