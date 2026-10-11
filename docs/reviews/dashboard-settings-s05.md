# S05: path-less episode metadata providers

A TV episode with no file of its own (a virtual or missing episode) refreshes
through `LocalProviderManager`, not the library scan. Its refresh ran
TheMovieDb's episode lookup only — and resolved the series by a *name search*
when no `Tmdb` id was stored — dropping OMDb's and the TVDB plugin's episode
providers, the library's provider order, the episode's own credits, and the
episode's ids. A TODO in the code named exactly that gap.

Source oracle: Jellyfin `4910aafa1a` `TmdbEpisodeProvider`
(`TmdbEpisodeProvider.cs:92-97`: the series' `Tmdb` id, no search),
`OmdbEpisodeProvider`, jellyfin-plugin-tvdb `5c4592f` `TvdbEpisodeProvider`,
`MetadataService.ExecuteRemoteProviders`/`RefreshWithProviders`
(`MetadataService.cs:955-1025`, `:272-293`).

## Behavior

- New `ferrofin-providers/src/episode.rs`, the episode counterpart of
  `season.rs`: `tmdb_answer`, `omdb_answer` (+ `OmdbGates`) and `tvdb_answer`
  turn each client's answer into the provider's `MetadataResult` — a fresh item
  with only what the provider sets, its credits and its ids. TheMovieDb: numbers,
  title, synopsis, rating (> 0), air date and year, the episode's own `Tmdb` id
  and the `Imdb`/`Tvdb`/`TvRage` ids of its external ids, its trailers, and its
  credits (a null list when TMDB credits nobody or the request missed). OMDb:
  title/certificate/genres only for an English library (certificate US only),
  original language, home page, plot, ratings, year, `Imdb` id, credits only with
  the plugin's `CastAndCrew`. TheTVDB: numbers, original title, the localized
  title/synopsis, air date, the specials' `Airs*` placement for the aired order,
  the episode's `Tvdb`/`Imdb` ids and its own credits (a list even when empty).
  `omdb_people`/`tvdb_people` moved here; the scanner calls them.
- `LocalProviderManager` registers TheTVDB and OMDb as episode fetchers (the
  latter only with a key) beside TheMovieDb, and a path-less episode's refresh
  (`refresh_item`) now runs `fetch_episode_providers`: the providers the library
  has enabled, in its saved order (default registration order breaks ties),
  Identify's chosen provider first. Every provider resolves by the **series'
  stored ids** — `Tmdb`, `Imdb`, `Tvdb` — never by a name search; a provider the
  series has no id for is not asked. OMDb also reads the episode's own `Imdb` id
  from the providers that ran before it.
- The answers fold as `ExecuteRemoteProviders` folds them: the first answer wins
  a field, later ones fill what is empty (lists union), ids gather, an overview in
  the preferred language displaces a fallback in another (OMDb's English plot),
  each answer's credits are kept and written by the usual rule (a `Cast` lock
  keeps the stored ones). An identified episode (Apply) looks up by the chosen
  result's ids as its series' and has none of its own. All failures are counted
  per request, so a failing provider leaves the refresh unstamped and never
  clears what the episode has.
- `fetch_tv`/`fetch_season` (the TMDB-only, name-searching arm) are gone; the
  TODO with them.

## Verification

- `episode.rs` unit tests over the three mappers (empty synopsis/zero rating not
  set, trailers and credits, language and region gates, `CastAndCrew`, aired-order
  placement, empty credit lists).
- Manager tests through mock TheMovieDb/TheTVDB/OMDb servers over real HTTP: all
  three providers in both libraries' orders (first answer wins title/synopsis/date,
  the first `Imdb` id wins, every other id gathers, no `/search` request); a
  library with one provider and a series with no ids (nothing asked); credits
  written and a `Cast` lock; a French library taking TheTVDB's French synopsis over
  OMDb's English plot that ran first; every provider failing under Replace all
  (nothing erased, not stamped).
- Production binary smoke (a path-less episode, providers unreachable as the
  fixture has no network): `POST /Items/{id}/Refresh` with Replace all returns
  `204`, the episode keeps its title and synopsis, the server stays healthy —
  same before and after. The server's provider endpoints are a test seam that is
  never set from a file or environment, so the provider answers themselves are
  verified by the manager tests above, not through the binary.

## Limitations

- The TVDB plugin's `IsAutomated` branch for episodes
  (`TvdbEpisodeProvider.cs:171`, an item-refresh route detail shared with the
  season TODO) is unchanged.
- `Fetched` images for an episode still come from the ordered image pass
  (`refresh_ordered_images`); this change is metadata only.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on the providers crate passed: providers 926 (4 skipped), core
2,484, livetv 299, server 237, extensions 96, wasm 62; real FFmpeg tests
enabled, the WASM guest build disabled. Clippy first flagged the new episode fold
as over-long and the two now-identical `RefreshTarget` arms; the OMDb and TVDB
branches became `omdb_episode`/`tvdb_episode` and the arms were merged, then fmt,
Clippy, the boundary test and every package's tests and doctests were rerun
clean. Separate line coverage over each changed crate's own sources, measured
before that refactor (a move of code with its tests unchanged): providers
**92.05%**, core **95.15%**, both above the 80% gate; the server composition root
is coverage-exempt. Instrumented builds were cleaned (`target` about 25 GiB).
