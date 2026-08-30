//! The [`ProviderManager`] trait implementation — port of the
//! `MediaBrowser.Providers.Manager.ProviderManager` surface.
//!
//! Scope note: the C# `ProviderManager` couples metadata *refresh orchestration*
//! to the library item store and the image-saving pipeline. Ferrofin splits that
//! — the scan/refresh pipeline lives in `ferrofin-core`'s library scanner, and
//! this type carries the client-facing surface: remote search ("Identify"),
//! remote images ("Choose Image"), the external-id descriptor set, and the
//! external-URL ("Links") table.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ferrofin_model::configuration::{MetadataOptions, MetadataPluginSummary};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::ImageType;
use ferrofin_model::net::mime_types;
use ferrofin_model::providers::{
    ExternalIdInfo, ImageProviderInfo, RemoteImageInfo, RemoteImageQuery, RemoteSearchResult,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository, ItemTypeLookup};
use ferrofin_traits::providers::{
    ItemUpdateType, MetadataRefreshMode, MetadataRefreshOptions, ProviderManager, RefreshPriority,
    RemoteSearchRequest,
};
use uuid::Uuid;

use crate::error::ProvidersError;
use crate::library_options::{fetcher_names, image_fetcher_enabled, metadata_fetcher_enabled};
use crate::tmdb::{TmdbClient, TmdbDetails, TmdbImage, TmdbKind};
use ferrofin_db::entities::base_items::BaseItemEntity;

/// A [`RemoteSearchProvider`] backed by TMDB (the "Identify" flow). One instance
/// searches a single kind (movie or series), so it is registered once per kind.
pub struct TmdbSearchProvider {
    tmdb: Arc<TmdbClient>,
    kind: TmdbKind,
    supported: BaseItemKind,
}

impl TmdbSearchProvider {
    /// A TMDB search provider for `kind` (`Movie` or `Series`).
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>, kind: TmdbKind) -> Self {
        let supported = match kind {
            TmdbKind::Movie => BaseItemKind::Movie,
            TmdbKind::Series => BaseItemKind::Series,
        };
        Self {
            tmdb,
            kind,
            supported,
        }
    }
}

#[async_trait]
impl RemoteSearchProvider for TmdbSearchProvider {
    // The trait fixes the return as `&str`; the literal is unavoidably behind it.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "TheMovieDb"
    }

    fn supports(&self, item_kind: BaseItemKind) -> bool {
        item_kind == self.supported
    }

    async fn get_search_results(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        let search_info = &request.search_info;
        let Some(name) = search_info.name.as_deref().filter(|n| !n.is_empty()) else {
            return Ok(Vec::new());
        };
        let hits = self.tmdb.search(self.kind, name, search_info.year).await;
        Ok(hits
            .into_iter()
            .map(|hit| RemoteSearchResult {
                name: hit.name,
                production_year: hit.year,
                image_url: hit.poster_url,
                overview: hit.overview,
                provider_ids: Some(std::collections::HashMap::from([(
                    "Tmdb".to_owned(),
                    hit.tmdb_id.to_string(),
                )])),
                search_provider_name: Some("TheMovieDb".to_owned()),
                ..RemoteSearchResult::default()
            })
            .collect())
    }
}

/// A [`RemoteSearchProvider`] backed by TheTVDB — the "Identify" flow for TV
/// series. Returns candidates carrying their `Tvdb` provider id.
pub struct TvdbSearchProvider {
    tvdb: Arc<crate::tvdb::TvdbClient>,
}

impl TvdbSearchProvider {
    /// A TVDB series search provider.
    #[must_use]
    pub fn new(tvdb: Arc<crate::tvdb::TvdbClient>) -> Self {
        Self { tvdb }
    }
}

#[async_trait]
impl RemoteSearchProvider for TvdbSearchProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "TheTVDB"
    }

    fn supports(&self, item_kind: BaseItemKind) -> bool {
        item_kind == BaseItemKind::Series
    }

    async fn get_search_results(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        let search_info = &request.search_info;
        let Some(name) = search_info.name.as_deref().filter(|n| !n.is_empty()) else {
            return Ok(Vec::new());
        };
        Ok(self
            .tvdb
            .search(name, search_info.year)
            .await
            .into_iter()
            .map(|hit| RemoteSearchResult {
                name: Some(hit.name),
                production_year: hit.year,
                image_url: hit.image_url,
                overview: hit.overview,
                provider_ids: Some(std::collections::HashMap::from([(
                    "Tvdb".to_owned(),
                    hit.tvdb_id.to_string(),
                )])),
                search_provider_name: Some("TheTVDB".to_owned()),
                ..RemoteSearchResult::default()
            })
            .collect())
    }
}

/// A [`RemoteSearchProvider`] over TMDB's *collections* — the "Identify" flow
/// for a box set. Port of `TmdbBoxSetProvider.GetSearchResults`.
pub struct TmdbBoxSetSearchProvider {
    tmdb: Arc<TmdbClient>,
}

impl TmdbBoxSetSearchProvider {
    /// A TMDB collection search provider.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self { tmdb }
    }
}

#[async_trait]
impl RemoteSearchProvider for TmdbBoxSetSearchProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "TheMovieDb"
    }

    fn supports(&self, item_kind: BaseItemKind) -> bool {
        item_kind == BaseItemKind::BoxSet
    }

    async fn get_search_results(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        let search_info = &request.search_info;
        let Some(name) = search_info.name.as_deref().filter(|n| !n.is_empty()) else {
            return Ok(Vec::new());
        };
        Ok(self
            .tmdb
            .search_collection(name)
            .await
            .into_iter()
            .map(|hit| RemoteSearchResult {
                name: Some(hit.name),
                overview: hit.overview,
                image_url: hit.poster_url,
                // C# `TmdbBoxSetProvider` sets `MetadataProvider.Tmdb` on the
                // box set itself — `TmdbCollection` is the key a *movie* uses
                // to point at its collection, and is not read back for a
                // BoxSet by the links table or the image path.
                provider_ids: Some(std::collections::HashMap::from([(
                    "Tmdb".to_owned(),
                    hit.tmdb_id.to_string(),
                )])),
                search_provider_name: Some("TheMovieDb".to_owned()),
                ..RemoteSearchResult::default()
            })
            .collect())
    }
}

/// A [`RemoteSearchProvider`] backed by OMDb — the "Identify" flow's IMDb-keyed
/// candidates. Port of `OmdbItemProvider.GetSearchResults`; inert (no results)
/// until an OMDb API key is configured.
pub struct OmdbSearchProvider {
    omdb: Arc<crate::omdb::OmdbClient>,
    kind: crate::omdb::OmdbKind,
    supported: BaseItemKind,
}

impl OmdbSearchProvider {
    /// An OMDb search provider for `kind` (movie, series or episode).
    #[must_use]
    pub fn new(omdb: Arc<crate::omdb::OmdbClient>, kind: crate::omdb::OmdbKind) -> Self {
        let supported = match kind {
            crate::omdb::OmdbKind::Movie => BaseItemKind::Movie,
            crate::omdb::OmdbKind::Series => BaseItemKind::Series,
            crate::omdb::OmdbKind::Episode => BaseItemKind::Episode,
        };
        Self {
            omdb,
            kind,
            supported,
        }
    }

    /// An OMDb search provider for trailers — the
    /// `IRemoteMetadataProvider<Trailer, TrailerInfo>` face of
    /// `OmdbItemProvider`, whose `GetSearchResultsInternal` maps a
    /// `TrailerInfo` to OMDb's `type=movie` (the `_ => "movie"` arm).
    #[must_use]
    pub fn for_trailers(omdb: Arc<crate::omdb::OmdbClient>) -> Self {
        Self {
            omdb,
            kind: crate::omdb::OmdbKind::Movie,
            supported: BaseItemKind::Trailer,
        }
    }
}

#[async_trait]
impl RemoteSearchProvider for OmdbSearchProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "The Open Movie Database"
    }

    fn supports(&self, item_kind: BaseItemKind) -> bool {
        item_kind == self.supported
    }

    async fn get_search_results(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        let search_info = &request.search_info;
        // An id already on the item resolves it exactly; only a nameless item
        // with no id has nothing to search on at all.
        // Season/Episode narrow the query ONLY for an episode search; on a
        // movie or series they would ask OMDb for a record that does not
        // exist. Note that an episode search additionally needs the SERIES'
        // IMDb id (C# reads `SeriesProviderIds`, not the episode's own, because
        // OMDb keys a season listing by the series) — `ItemLookupInfo` carries
        // no such field and no `OmdbKind::Episode` provider is registered, so
        // that branch is unreachable until both exist.
        let is_episode = self.kind == crate::OmdbKind::Episode;
        let known = crate::OmdbSearchKey {
            imdb_id: search_info
                .provider_ids
                .as_ref()
                .and_then(|ids| ids.iter().find(|(key, _)| key.eq_ignore_ascii_case("Imdb")))
                .map(|(_, value)| value.as_str())
                .filter(|_| !is_episode),
            season: is_episode
                .then_some(search_info.parent_index_number)
                .flatten(),
            episode: is_episode.then_some(search_info.index_number).flatten(),
        };
        let name = search_info.name.as_deref().unwrap_or_default();
        if name.is_empty() && known.imdb_id.is_none() {
            return Ok(Vec::new());
        }
        Ok(self
            .omdb
            .search(self.kind, name, search_info.year, &known)
            .await
            .into_iter()
            .map(|hit| RemoteSearchResult {
                production_year: hit.production_year(),
                premiere_date: hit.premiere_date(),
                image_url: hit.poster.clone(),
                provider_ids: hit
                    .imdb_id
                    .clone()
                    .map(|id| std::collections::HashMap::from([("Imdb".to_owned(), id)])),
                name: hit.title,
                search_provider_name: Some("The Open Movie Database".to_owned()),
                ..RemoteSearchResult::default()
            })
            .collect())
    }
}

/// The display name of the MusicBrainz providers (`Plugin.Name`).
const MUSICBRAINZ_PROVIDER_NAME: &str = fetcher_names::MUSICBRAINZ;
/// The display name of the TheAudioDb providers.
const AUDIODB_PROVIDER_NAME: &str = fetcher_names::AUDIODB;
/// The display name of the TMDB providers (`TmdbUtils.ProviderName`).
const TMDB_PROVIDER_NAME: &str = fetcher_names::TMDB;
/// The display name of the fanart.tv providers.
const FANART_PROVIDER_NAME: &str = fetcher_names::FANART;
/// The display name of the OMDb providers.
const OMDB_PROVIDER_NAME: &str = fetcher_names::OMDB;

/// A case-insensitive, non-empty provider-id lookup — the C# provider-id
/// dictionaries are `OrdinalIgnoreCase` and every `Get*Id` helper treats an
/// empty value as absent.
fn provider_id_of<'a>(ids: Option<&'a HashMap<String, String>>, key: &str) -> Option<&'a str> {
    ids?.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.trim().is_empty())
}

/// `AlbumInfoExtensions.GetReleaseId` / `GetReleaseGroupId`: the album's own
/// `key`, else the first contained song that carries one.
fn album_id_or_song_id(request: &RemoteSearchRequest, key: &str) -> Option<String> {
    provider_id_of(request.search_info.provider_ids.as_ref(), key)
        .or_else(|| {
            request
                .song_infos
                .iter()
                .find_map(|song| provider_id_of(song.base.provider_ids.as_ref(), key))
        })
        .map(str::to_owned)
}

/// `AlbumInfoExtensions.GetMusicBrainzArtistId(AlbumInfo)`: the album's
/// `MusicBrainzAlbumArtist`, else the artist's `MusicBrainzArtist` (from
/// `ArtistProviderIds`), else the first song's `MusicBrainzAlbumArtist`.
fn album_artist_mbid(request: &RemoteSearchRequest) -> Option<String> {
    provider_id_of(
        request.search_info.provider_ids.as_ref(),
        "MusicBrainzAlbumArtist",
    )
    .or_else(|| provider_id_of(request.artist_provider_ids.as_ref(), "MusicBrainzArtist"))
    .or_else(|| {
        request.song_infos.iter().find_map(|song| {
            provider_id_of(song.base.provider_ids.as_ref(), "MusicBrainzAlbumArtist")
        })
    })
    .map(str::to_owned)
}

/// `AlbumInfoExtensions.GetAlbumArtist`: the first non-empty album artist
/// across the contained songs, else the album's own first album artist.
fn album_artist_name(request: &RemoteSearchRequest) -> Option<&str> {
    request
        .song_infos
        .iter()
        .flat_map(|song| song.album_artists.iter())
        .map(String::as_str)
        .find(|name| !name.is_empty())
        .or_else(|| request.album_artists.first().map(String::as_str))
}

/// `AlbumInfoExtensions.GetMusicBrainzArtistId(ArtistInfo)`: the artist's own
/// `MusicBrainzArtist`, else the first song's `MusicBrainzAlbumArtist`.
fn artist_mbid(request: &RemoteSearchRequest) -> Option<String> {
    provider_id_of(
        request.search_info.provider_ids.as_ref(),
        "MusicBrainzArtist",
    )
    .or_else(|| {
        request.song_infos.iter().find_map(|song| {
            provider_id_of(song.base.provider_ids.as_ref(), "MusicBrainzAlbumArtist")
        })
    })
    .map(str::to_owned)
}

/// Maps one MusicBrainz release hit into the "Identify" result shape — port of
/// `MusicBrainzAlbumProvider.GetReleaseResult`: title/date, the artist credits
/// (first = album artist, each with its `MusicBrainzArtist` id), and the
/// `MusicBrainzAlbum` + `MusicBrainzReleaseGroup` ids.
fn release_search_result(hit: crate::musicbrainz::ReleaseHit) -> RemoteSearchResult {
    let artists: Vec<RemoteSearchResult> = hit
        .artist_credits
        .into_iter()
        .map(|credit| RemoteSearchResult {
            name: credit.name,
            provider_ids: credit
                .artist_id
                .map(|id| HashMap::from([("MusicBrainzArtist".to_owned(), id)])),
            ..RemoteSearchResult::default()
        })
        .collect();
    let mut provider_ids = HashMap::from([("MusicBrainzAlbum".to_owned(), hit.id)]);
    if let Some(rg) = hit.release_group_id {
        provider_ids.insert("MusicBrainzReleaseGroup".to_owned(), rg);
    }
    RemoteSearchResult {
        name: hit.title,
        production_year: hit.date.map(|d| d.year),
        premiere_date: hit.date.and_then(crate::musicbrainz::PartialDate::to_utc),
        search_provider_name: Some(MUSICBRAINZ_PROVIDER_NAME.to_owned()),
        album_artist: artists.first().cloned().map(Box::new),
        artists,
        provider_ids: Some(provider_ids),
        ..RemoteSearchResult::default()
    }
}

/// Maps one MusicBrainz artist hit into the "Identify" result shape — port of
/// `MusicBrainzArtistProvider.GetResultFromResponse`.
fn artist_search_result(hit: crate::musicbrainz::ArtistHit) -> RemoteSearchResult {
    RemoteSearchResult {
        name: hit.name,
        production_year: hit.begin.map(|d| d.year),
        premiere_date: hit.begin.and_then(crate::musicbrainz::PartialDate::to_utc),
        search_provider_name: Some(MUSICBRAINZ_PROVIDER_NAME.to_owned()),
        provider_ids: Some(HashMap::from([("MusicBrainzArtist".to_owned(), hit.id)])),
        ..RemoteSearchResult::default()
    }
}

/// A [`RemoteSearchProvider`] over MusicBrainz releases — the "Identify" flow
/// for a `MusicAlbum`. Port of `MusicBrainzAlbumProvider.GetSearchResults`:
/// a known release id resolves exactly, a known release-group id expands to
/// its releases, else a lucene search by album title narrowed by the album
/// artist's MBID (`arid:`) or name (`artist:`).
pub struct MusicBrainzAlbumSearchProvider {
    musicbrainz: Arc<crate::musicbrainz::MusicBrainzClient>,
}

impl MusicBrainzAlbumSearchProvider {
    /// A MusicBrainz album search provider.
    #[must_use]
    pub fn new(musicbrainz: Arc<crate::musicbrainz::MusicBrainzClient>) -> Self {
        Self { musicbrainz }
    }
}

#[async_trait]
impl RemoteSearchProvider for MusicBrainzAlbumSearchProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        MUSICBRAINZ_PROVIDER_NAME
    }

    fn supports(&self, item_kind: BaseItemKind) -> bool {
        item_kind == BaseItemKind::MusicAlbum
    }

    async fn get_search_results(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        // A release id on the item resolves it exactly (`LookupReleaseAsync`).
        if let Some(release_id) = album_id_or_song_id(request, "MusicBrainzAlbum") {
            return Ok(self
                .musicbrainz
                .lookup_release(&release_id)
                .await
                .map(release_search_result)
                .into_iter()
                .collect());
        }
        // A release-group id expands to every release in the group.
        if let Some(group_id) = album_id_or_song_id(request, "MusicBrainzReleaseGroup") {
            return Ok(self
                .musicbrainz
                .release_group_releases(&group_id)
                .await
                .into_iter()
                .map(release_search_result)
                .collect());
        }
        let name = request.search_info.name.as_deref().unwrap_or_default();
        let query = if let Some(arid) = album_artist_mbid(request) {
            format!("\"{name}\" AND arid:{arid}")
        } else {
            // "resolves search for 12\" Mixes": strip embedded quotes from the
            // title before phrasing it. (The C# appends a stray `c` after the
            // artist phrase — a typo that would add a bare term to the lucene
            // query; not reproduced.)
            let query_name = name.replace('"', "");
            let artist = album_artist_name(request).unwrap_or_default();
            format!("\"{query_name}\" AND artist:\"{artist}\"")
        };
        Ok(self
            .musicbrainz
            .find_releases(&query)
            .await
            .into_iter()
            .map(release_search_result)
            .collect())
    }
}

/// A [`RemoteSearchProvider`] over MusicBrainz artists — the "Identify" flow
/// for a `MusicArtist`. Port of `MusicBrainzArtistProvider.GetSearchResults`:
/// a known artist id resolves exactly, else a phrase search by name, retried
/// through `artistaccent:` when the name carries diacritics.
pub struct MusicBrainzArtistSearchProvider {
    musicbrainz: Arc<crate::musicbrainz::MusicBrainzClient>,
}

impl MusicBrainzArtistSearchProvider {
    /// A MusicBrainz artist search provider.
    #[must_use]
    pub fn new(musicbrainz: Arc<crate::musicbrainz::MusicBrainzClient>) -> Self {
        Self { musicbrainz }
    }
}

#[async_trait]
impl RemoteSearchProvider for MusicBrainzArtistSearchProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        MUSICBRAINZ_PROVIDER_NAME
    }

    fn supports(&self, item_kind: BaseItemKind) -> bool {
        item_kind == BaseItemKind::MusicArtist
    }

    async fn get_search_results(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        if let Some(artist_id) = artist_mbid(request) {
            return Ok(self
                .musicbrainz
                .lookup_artist(&artist_id)
                .await
                .map(artist_search_result)
                .into_iter()
                .collect());
        }
        let name = request.search_info.name.as_deref().unwrap_or_default();
        let hits = self.musicbrainz.find_artists(&format!("\"{name}\"")).await;
        if !hits.is_empty() {
            return Ok(hits.into_iter().map(artist_search_result).collect());
        }
        if ferrofin_util::string_extensions::has_diacritics(name) {
            // Retry through the accent-preserving field.
            let hits = self
                .musicbrainz
                .find_artists(&format!("artistaccent:\"{name}\""))
                .await;
            return Ok(hits.into_iter().map(artist_search_result).collect());
        }
        Ok(Vec::new())
    }
}

/// The TheAudioDb face of the "Identify" flow for a `MusicAlbum` /
/// `MusicArtist`. Port of `AudioDbAlbumProvider.GetSearchResults` /
/// `AudioDbArtistProvider.GetSearchResults`, which both return an empty set —
/// TheAudioDb has no name search, it is keyed by MusicBrainz id and only
/// contributes during the metadata fetch. Registered so the provider is
/// selectable by name exactly as it is in Jellyfin.
pub struct AudioDbSearchProvider {
    supported: BaseItemKind,
}

impl AudioDbSearchProvider {
    /// A TheAudioDb search provider for `kind` (`MusicAlbum` or `MusicArtist`).
    #[must_use]
    pub fn new(kind: BaseItemKind) -> Self {
        Self { supported: kind }
    }
}

#[async_trait]
impl RemoteSearchProvider for AudioDbSearchProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        AUDIODB_PROVIDER_NAME
    }

    fn supports(&self, item_kind: BaseItemKind) -> bool {
        item_kind == self.supported
    }

    async fn get_search_results(
        &self,
        _request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        // `Task.FromResult(Enumerable.Empty<RemoteSearchResult>())`.
        Ok(Vec::new())
    }
}

/// A [`RemoteSearchProvider`] over TMDB's people — the "Identify" flow for a
/// `Person`. Port of `TmdbPersonProvider.GetSearchResults`: a `Tmdb` id on
/// the item resolves it exactly (name/biography/profile/IMDb id), else a
/// `/search/person` by name.
pub struct TmdbPersonSearchProvider {
    tmdb: Arc<TmdbClient>,
}

impl TmdbPersonSearchProvider {
    /// A TMDB person search provider.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self { tmdb }
    }
}

#[async_trait]
impl RemoteSearchProvider for TmdbPersonSearchProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        TMDB_PROVIDER_NAME
    }

    fn supports(&self, item_kind: BaseItemKind) -> bool {
        item_kind == BaseItemKind::Person
    }

    async fn get_search_results(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        let search_info = &request.search_info;
        if let Some(tmdb_id) = provider_id_of(search_info.provider_ids.as_ref(), "Tmdb")
            .and_then(|id| id.trim().parse::<i64>().ok())
            && let Some(person) = self.tmdb.person_lookup(tmdb_id).await
        {
            let mut provider_ids = HashMap::from([("Tmdb".to_owned(), person.tmdb_id.to_string())]);
            // `TrySetProviderId(Imdb, …)` — only when TMDB knows it.
            if let Some(imdb) = person.imdb_id {
                provider_ids.insert("Imdb".to_owned(), imdb);
            }
            return Ok(vec![RemoteSearchResult {
                name: person.name,
                overview: person.biography,
                image_url: person.profile_url,
                provider_ids: Some(provider_ids),
                search_provider_name: Some(TMDB_PROVIDER_NAME.to_owned()),
                ..RemoteSearchResult::default()
            }]);
        }
        let Some(name) = search_info.name.as_deref().filter(|n| !n.trim().is_empty()) else {
            return Ok(Vec::new());
        };
        Ok(self
            .tmdb
            .search_person(name)
            .await
            .into_iter()
            .map(|person| RemoteSearchResult {
                name: person.name,
                image_url: person.profile_url,
                provider_ids: Some(HashMap::from([(
                    "Tmdb".to_owned(),
                    person.tmdb_id.to_string(),
                )])),
                search_provider_name: Some(TMDB_PROVIDER_NAME.to_owned()),
                ..RemoteSearchResult::default()
            })
            .collect())
    }
}

/// A single remote metadata-search fetcher (e.g. a TMDb or MusicBrainz plugin).
///
/// Port of `MediaBrowser.Controller.Providers.IRemoteSearchProvider<T>` reduced
/// to the object-safe surface the manager needs: a display name, the item kinds
/// it serves, and the search itself. The concrete network fetchers (TMDB, TVDB,
/// OMDb, MusicBrainz) implement it in this crate and are registered by the
/// server's composition root; the dedup/merge port in
/// [`LocalProviderManager::remote_search`] drives them all.
#[async_trait]
pub trait RemoteSearchProvider: Send + Sync {
    /// The provider's display name, stamped onto every result it returns.
    fn name(&self) -> &str;

    /// Whether this provider can search for `item_kind`.
    fn supports(&self, item_kind: BaseItemKind) -> bool;

    /// Runs the search, returning raw candidate results (name/provider-ids set).
    /// `request` carries the shared
    /// [`ItemLookupInfo`](ferrofin_model::providers::ItemLookupInfo)
    /// (`search_info`) plus the
    /// type-specific fields (album artists, song infos, …) a kind's C#
    /// `GetSearchResults` overload reads.
    ///
    /// # Errors
    ///
    /// Whatever the concrete fetcher surfaces; the manager logs and continues on
    /// a per-provider error rather than failing the whole search.
    async fn get_search_results(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError>;
}

/// The [`ProviderManager`]: remote search ("Identify"), remote images ("Choose
/// Image"), item refresh (TMDB-backed, honouring stored/chosen provider ids),
/// image save/delete, and the external-id descriptor set.
///
/// Construct via [`LocalProviderManager::new`] and attach the clients/stores
/// with the `with_*` builders; anything not attached simply contributes
/// nothing (search providers, image providers) or reports itself unwired
/// (image writes). The external-id descriptors it advertises can be supplied
/// at construction so downstream NFO parsing sees the same provider set the
/// manager would.
#[derive(Default, Clone)]
pub struct LocalProviderManager {
    external_id_infos: Vec<ExternalIdInfo>,
    /// Runtime-registered named metadata providers (WASM plugins) as
    /// (name, supported kinds), surfaced in library options.
    dynamic_fetchers: Vec<(String, Vec<String>)>,
    remote_search_providers: Vec<Arc<dyn RemoteSearchProvider>>,
    /// The item-image store (rows) + the directory uploaded images are written
    /// to. Present enables the `save_image`/`delete_image` write paths.
    image_store: Option<Arc<dyn ItemPersistenceService>>,
    metadata_dir: Option<PathBuf>,
    /// The TMDB client + item store used by the remote-image ("Choose Image")
    /// methods to resolve an item and list/download its TMDB artwork.
    tmdb: Option<Arc<TmdbClient>>,
    items: Option<Arc<dyn ItemRepository>>,
    /// The virtual-folder manager, used to resolve the owning library's saved
    /// `LibraryOptions` for an item so a refresh honours its metadata/image
    /// fetcher checkboxes (C# `ProviderManager.CanRefreshMetadata` /
    /// `CanRefreshImages` -> `BaseItemManager`). Absent → no library is
    /// resolvable and the built-in defaults stand.
    virtual_folders: Option<Arc<dyn ferrofin_traits::library::VirtualFolderManager>>,
    /// The Studio Images client, used to supply a `Studio` item's thumb from the
    /// artwork repository. Absent → studios contribute no remote images.
    studios: Option<Arc<crate::studios::StudiosClient>>,
    /// fanart.tv — movie/series/artist/album artwork keyed by the item's
    /// stored Tmdb/Imdb/Tvdb/MusicBrainz ids. Absent → not a remote image
    /// provider.
    fanart: Option<Arc<crate::fanart::FanartClient>>,
    /// TheAudioDb — artist/album artwork keyed by MusicBrainz ids.
    audiodb: Option<Arc<crate::audiodb::AudioDbClient>>,
    /// OMDb — the poster (Primary) for a movie/trailer/episode with an IMDb id.
    omdb: Option<Arc<crate::omdb::OmdbClient>>,
    /// Stored `BaseItems.Type` name → [`BaseItemKind`], inverted once from the
    /// [`ItemTypeLookup`] table. Present enables the kind-filtered built-in
    /// external-id descriptors.
    kind_by_type_name: HashMap<String, BaseItemKind>,
}

impl std::fmt::Debug for LocalProviderManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalProviderManager")
            .field("external_id_infos", &self.external_id_infos)
            .field(
                "remote_search_providers",
                &self.remote_search_providers.len(),
            )
            .field("has_image_store", &self.image_store.is_some())
            .field("metadata_dir", &self.metadata_dir)
            .field("has_tmdb", &self.tmdb.is_some())
            .field("has_items", &self.items.is_some())
            .field("has_virtual_folders", &self.virtual_folders.is_some())
            .field("has_studios", &self.studios.is_some())
            .field("has_fanart", &self.fanart.is_some())
            .field("has_audiodb", &self.audiodb.is_some())
            .field("has_omdb", &self.omdb.is_some())
            .field("dynamic_fetchers", &self.dynamic_fetchers.len())
            .field("kind_by_type_name", &self.kind_by_type_name.len())
            .finish()
    }
}

impl LocalProviderManager {
    /// Creates a manager advertising `external_id_infos` for every item.
    ///
    /// No remote-search providers are registered yet, so
    /// [`remote_search`](Self::remote_search) returns `[]` until
    /// [`with_remote_search_providers`](Self::with_remote_search_providers)
    /// supplies the fetchers.
    #[must_use]
    pub fn new(external_id_infos: Vec<ExternalIdInfo>) -> Self {
        Self {
            external_id_infos,
            dynamic_fetchers: Vec::new(),
            remote_search_providers: Vec::new(),
            image_store: None,
            metadata_dir: None,
            tmdb: None,
            items: None,
            virtual_folders: None,
            studios: None,
            fanart: None,
            audiodb: None,
            omdb: None,
            kind_by_type_name: HashMap::new(),
        }
    }

    /// Attaches the fanart.tv client as a remote image provider for movies,
    /// series, artists and albums (keyed by their stored external ids).
    #[must_use]
    pub fn with_fanart(mut self, fanart: Arc<crate::fanart::FanartClient>) -> Self {
        self.fanart = Some(fanart);
        self
    }

    /// Attaches the TheAudioDb client as a remote image provider for artists
    /// and albums (keyed by their stored MusicBrainz ids).
    #[must_use]
    pub fn with_audiodb(mut self, audiodb: Arc<crate::audiodb::AudioDbClient>) -> Self {
        self.audiodb = Some(audiodb);
        self
    }

    /// Attaches the OMDb client as a remote image provider (the poster of a
    /// movie/trailer/episode with an IMDb id). Inert without an API key.
    #[must_use]
    pub fn with_omdb(mut self, omdb: Arc<crate::omdb::OmdbClient>) -> Self {
        self.omdb = Some(omdb);
        self
    }

    /// Enables the kind-filtered built-in external-id descriptors by supplying
    /// the item-type table (inverted once here) — a port of the C#
    /// `IExternalId.Supports(item)` filter, which needs the item's type.
    ///
    /// Without it (and without an item store) only the descriptors handed to
    /// [`new`](Self::new) are advertised.
    #[must_use]
    pub fn with_item_types(mut self, types: &dyn ItemTypeLookup) -> Self {
        self.kind_by_type_name = types
            .base_item_kind_names()
            .into_iter()
            .map(|(kind, name)| (name, kind))
            .collect();
        self
    }

    /// Attaches the Studio Images client, so a `Studio` item's remote images
    /// include the artwork-repository thumb. Absent, studios contribute nothing.
    #[must_use]
    pub fn with_studios(mut self, studios: Arc<crate::studios::StudiosClient>) -> Self {
        self.studios = Some(studios);
        self
    }

    /// Attaches the TMDB client + item store used by the remote-image methods
    /// (`get_available_remote_images` / `save_image_from_url`) and the item
    /// refresh. Absent, those return empty / report themselves unwired.
    #[must_use]
    pub fn with_remote_images(
        mut self,
        tmdb: Arc<TmdbClient>,
        items: Arc<dyn ItemRepository>,
    ) -> Self {
        self.tmdb = Some(tmdb);
        self.items = Some(items);
        self
    }

    /// Registers runtime-loaded named metadata providers (WASM plugins)
    /// so the dashboard's library-options fetcher lists include them.
    #[must_use]
    pub fn with_dynamic_fetchers(mut self, fetchers: Vec<(String, Vec<String>)>) -> Self {
        self.dynamic_fetchers = fetchers;
        self
    }

    /// Registers the remote-search fetchers this manager queries.
    ///
    /// The default set is empty; the server's composition root supplies the
    /// TMDB/TVDB/OMDb/MusicBrainz/TheAudioDb fetchers here.
    #[must_use]
    pub fn with_remote_search_providers(
        mut self,
        providers: Vec<Arc<dyn RemoteSearchProvider>>,
    ) -> Self {
        self.remote_search_providers = providers;
        self
    }

    /// Attaches the item-image store + the directory uploaded images are written
    /// to, enabling [`save_image`](ProviderManager::save_image) /
    /// [`delete_image`](ProviderManager::delete_image) and the refresh's
    /// persistence. Absent, the image writes report themselves unwired (unit
    /// tests / hosts without an image store).
    #[must_use]
    pub fn with_image_store(
        mut self,
        image_store: Arc<dyn ItemPersistenceService>,
        metadata_dir: PathBuf,
    ) -> Self {
        self.image_store = Some(image_store);
        self.metadata_dir = Some(metadata_dir);
        self
    }

    /// Attaches the virtual-folder manager so an on-demand refresh can read the
    /// owning library's `LibraryOptions` and honour its fetcher checkboxes.
    ///
    /// Without it a `POST /Items/{id}/Refresh` downloads remote metadata and
    /// artwork into a library whose "Metadata downloaders" / "Image fetchers"
    /// boxes are all cleared — the scan honours them, so the two paths would
    /// disagree about the same library.
    #[must_use]
    pub fn with_virtual_folders(
        mut self,
        virtual_folders: Arc<dyn ferrofin_traits::library::VirtualFolderManager>,
    ) -> Self {
        self.virtual_folders = Some(virtual_folders);
        self
    }

    /// The saved `LibraryOptions` of the library that owns `entity`, resolved
    /// through its `TopParentId` (the collection-folder id every scanned row
    /// carries). `None` when no library manager is wired, the row has no top
    /// parent, or the library saved no options — all of which mean "no
    /// customisation", i.e. the built-in defaults.
    async fn library_options_for(
        &self,
        entity: &BaseItemEntity,
    ) -> Option<ferrofin_model::configuration::LibraryOptions> {
        let folders = self.virtual_folders.as_ref()?;
        let top = entity
            .top_parent_id
            .as_deref()
            .and_then(|raw| Uuid::parse_str(raw).ok())?;
        let folders = folders.get_virtual_folders().await.ok()?;
        folders.into_iter().find_map(|folder| {
            let id = folder
                .item_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok());
            (id == Some(top))
                .then_some(folder.library_options)
                .flatten()
        })
    }

    /// `options` with the refresh modes forced to `None` wherever the C# gate
    /// would refuse the provider.
    ///
    /// - metadata: `CanRefreshMetadata` — "If locked only allow local
    ///   providers", then `IsMetadataFetcherEnabled`.
    /// - images: `CanRefreshImages` — refused when the item is locked and the
    ///   mode is not `FullRefresh`, then `IsImageFetcherEnabled`.
    ///
    /// TMDB is the only remote provider this manager's refresh path drives, so
    /// it is the only fetcher name consulted; adding another provider here
    /// means gating it by its own [`fetcher_names`] entry.
    async fn gated_options(
        &self,
        entity: &BaseItemEntity,
        options: &MetadataRefreshOptions,
    ) -> MetadataRefreshOptions {
        let library = self.library_options_for(entity).await;
        let kind = short_kind(entity);
        let metadata_allowed = !entity.is_locked
            && metadata_fetcher_enabled(library.as_ref(), kind, fetcher_names::TMDB);
        let images_allowed = (!entity.is_locked
            || options.image_refresh_mode == MetadataRefreshMode::FullRefresh)
            && image_fetcher_enabled(library.as_ref(), kind, fetcher_names::TMDB);
        MetadataRefreshOptions {
            metadata_refresh_mode: if metadata_allowed {
                options.metadata_refresh_mode
            } else {
                MetadataRefreshMode::None
            },
            image_refresh_mode: if images_allowed {
                options.image_refresh_mode
            } else {
                MetadataRefreshMode::None
            },
            replace_all_metadata: options.replace_all_metadata,
            replace_all_images: options.replace_all_images,
            search_result: options.search_result.clone(),
        }
    }

    /// Resolves what a refresh should fetch for `entity`: movies/series search
    /// TMDB by their own title; seasons/episodes hop to their parent series (its
    /// name/year drive the TMDB series search, the season/episode numbers select
    /// within it). Music and every other kind has no provider — `None`, the
    /// faithful skip.
    async fn resolve_refresh_target(
        &self,
        items: &Arc<dyn ItemRepository>,
        entity: &BaseItemEntity,
    ) -> Result<Option<RefreshTarget>, ServiceError> {
        // Only the season/episode arms need the parent-series row; fetch it here
        // so the classification itself stays pure (unit-testable without a repo).
        let series = if matches!(short_kind(entity), "Season" | "Episode") {
            match entity
                .series_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                Some(series_uuid) => items.retrieve_item(series_uuid).await?,
                None => None,
            }
        } else {
            None
        };
        Ok(refresh_target_of(entity, series.as_ref()))
    }

    /// Resolves the parent series' TMDB id (its stored provider ids first,
    /// then a title search) and fetches one season's details — the shared
    /// first half of the season/episode refresh arms. `None` when the series
    /// has no TMDB match or the season fetch fails.
    async fn fetch_season(
        &self,
        tmdb: &Arc<TmdbClient>,
        series_id: Option<Uuid>,
        series_name: &str,
        series_year: Option<i32>,
        season_number: i32,
    ) -> Option<crate::tmdb::SeasonDetails> {
        let stored = match series_id {
            Some(id) => self.stored_provider_ids(id).await,
            None => Vec::new(),
        };
        let tmdb_id = match resolve_tmdb_id(tmdb, TmdbKind::Series, &stored).await {
            Some(id) => id,
            None => {
                tmdb.search(TmdbKind::Series, series_name, series_year)
                    .await
                    .into_iter()
                    .next()?
                    .tmdb_id
            }
        };
        tmdb.season_details(tmdb_id, season_number).await
    }

    /// The item's stored external ids (`BaseItemProviders`) as
    /// `(key, value)` pairs; empty without a store or when none are stored.
    async fn stored_provider_ids(&self, item_id: Uuid) -> Vec<(String, String)> {
        let Some(store) = &self.image_store else {
            return Vec::new();
        };
        match store.provider_ids_for_items(&[item_id]).await {
            Ok(mut map) => map.remove(&item_id).unwrap_or_default(),
            Err(err) => {
                tracing::warn!(%item_id, %err, "could not read the item's provider ids");
                Vec::new()
            }
        }
    }

    /// The box-set refresh arm — port of `TmdbBoxSetProvider` +
    /// `TmdbBoxSetImageProvider`: search TMDB's collections by the box set's
    /// name, take the top hit, and apply its name/overview and artwork.
    async fn refresh_box_set(
        &self,
        tmdb: &Arc<TmdbClient>,
        entity: &mut BaseItemEntity,
        item_id: Uuid,
        name: &str,
        collection_id: Option<i64>,
        options: &MetadataRefreshOptions,
    ) -> Result<bool, ServiceError> {
        // A `Tmdb` id already on the box set (or on the chosen Identify
        // result) pins the collection; else the name search's top hit.
        let collection_id = if let Some(id) = collection_id {
            id
        } else {
            let Some(hit) = tmdb.search_collection(name).await.into_iter().next() else {
                return Ok(false);
            };
            hit.tmdb_id
        };
        let Some(collection) = tmdb.collection(collection_id).await else {
            return Ok(false);
        };
        if wants_fetch(options.metadata_refresh_mode) {
            apply_name_overview(
                entity,
                Some(collection.name.as_str()),
                collection.overview.as_deref(),
                options.replace_all_metadata,
            );
            self.persist_refreshed(entity).await?;
            if let Some(store) = &self.image_store {
                store
                    .save_provider_id(item_id, "Tmdb", &collection_id.to_string())
                    .await?;
            }
        }
        let mut image_saved = false;
        if wants_fetch(options.image_refresh_mode) && self.image_store.is_some() {
            for image_type in [ImageType::Primary, ImageType::Backdrop] {
                if let Some(image) = collection
                    .images
                    .iter()
                    .find(|i| i.image_type == image_type)
                {
                    image_saved |= self
                        .save_image_from_url(item_id, &image.url, image_type, None)
                        .await
                        .is_ok();
                }
            }
        }
        Ok(image_saved)
    }

    /// Applies a season's/episode's/person's fetched name/overview (+ Primary
    /// artwork URL) onto the row — the shared second half of the TV/person
    /// refresh arms. The metadata pass persists via the item store; the image
    /// download is best-effort (a failed download must not fail the refresh).
    /// Returns whether the image was saved.
    #[allow(clippy::too_many_arguments)]
    async fn apply_tv_slice(
        &self,
        entity: &mut BaseItemEntity,
        item_id: Uuid,
        name: Option<&str>,
        overview: Option<&str>,
        image_url: Option<&str>,
        options: &MetadataRefreshOptions,
    ) -> Result<bool, ServiceError> {
        if wants_fetch(options.metadata_refresh_mode) {
            apply_name_overview(entity, name, overview, options.replace_all_metadata);
            self.persist_refreshed(entity).await?;
        }
        if wants_fetch(options.image_refresh_mode)
            && self.image_store.is_some()
            && let Some(url) = image_url
        {
            return Ok(self
                .save_image_from_url(item_id, url, ImageType::Primary, None)
                .await
                .is_ok());
        }
        Ok(false)
    }

    /// Persists a refreshed row through the item store, stamping
    /// `DateLastRefreshed` (C# `MetadataService.RefreshMetadata` sets it on
    /// every refresh, and the "refresh people" task keys off it).
    async fn persist_refreshed(&self, entity: &mut BaseItemEntity) -> Result<(), ServiceError> {
        if let Some(store) = &self.image_store {
            entity.date_last_refreshed = Some(Utc::now());
            store.save_items(std::slice::from_ref(entity)).await?;
        }
        Ok(())
    }

    /// The season/episode refresh arm: the parent series' season from TMDB,
    /// then the season itself or the one episode within it. Returns whether
    /// an image was saved; `None`-targets (no match) save nothing.
    async fn refresh_tv(
        &self,
        tmdb: &Arc<TmdbClient>,
        entity: &mut BaseItemEntity,
        item_id: Uuid,
        target: RefreshTarget,
        options: &MetadataRefreshOptions,
    ) -> Result<bool, ServiceError> {
        let (series_id, series_name, series_year, season_number, episode_number) = match target {
            RefreshTarget::Season {
                series_id,
                series_name,
                series_year,
                season_number,
            } => (series_id, series_name, series_year, season_number, None),
            RefreshTarget::Episode {
                series_id,
                series_name,
                series_year,
                season_number,
                episode_number,
            } => (
                series_id,
                series_name,
                series_year,
                season_number,
                Some(episode_number),
            ),
            RefreshTarget::Title { .. }
            | RefreshTarget::BoxSet { .. }
            | RefreshTarget::Person { .. } => {
                return Ok(false);
            }
        };
        let Some(season) = self
            .fetch_season(tmdb, series_id, &series_name, series_year, season_number)
            .await
        else {
            return Ok(false);
        };
        // A season's artwork is its poster, an episode's its still — both
        // stored as Primary, like the scanner.
        let (name, overview, image_url) = match episode_number {
            None => (season.name, season.overview, season.poster),
            Some(number) => {
                let Some(episode) = season
                    .episodes
                    .into_iter()
                    .find(|ep| ep.episode_number == number)
                else {
                    return Ok(false);
                };
                (episode.name, episode.overview, episode.still_url)
            }
        };
        self.apply_tv_slice(
            entity,
            item_id,
            name.as_deref(),
            overview.as_deref(),
            image_url.as_deref(),
            options,
        )
        .await
    }

    /// The movie/series refresh arm — port of `TmdbMovieProvider` /
    /// `TmdbSeriesProvider` + their image providers against a resolved TMDB
    /// id: apply the fetched details (and stamp the Tmdb/Imdb ids), then
    /// download the primary + backdrop. Returns whether an image was saved.
    async fn refresh_title(
        &self,
        tmdb: &Arc<TmdbClient>,
        entity: &mut BaseItemEntity,
        item_id: Uuid,
        kind: TmdbKind,
        tmdb_id: i64,
        options: &MetadataRefreshOptions,
    ) -> Result<bool, ServiceError> {
        let Some(details) = tmdb.details(kind, tmdb_id).await else {
            return Ok(false);
        };
        // Metadata pass: apply the fetched fields onto the row and persist
        // through the item store (the same `ItemPersistenceService` the
        // scanner writes enriched rows with).
        if wants_fetch(options.metadata_refresh_mode) {
            apply_tmdb_details(entity, &details, options.replace_all_metadata);
            self.persist_refreshed(entity).await?;
            // The fetch's own ids (TMDB + its IMDb id) join the set, as the C#
            // `SetProviderId` calls in the provider do.
            if let Some(store) = &self.image_store {
                store
                    .save_provider_id(item_id, "Tmdb", &tmdb_id.to_string())
                    .await?;
                if let Some(imdb) = details.imdb_id.as_deref().filter(|s| !s.is_empty()) {
                    store.save_provider_id(item_id, "Imdb", imdb).await?;
                }
            }
        }
        // Image pass: download the primary + backdrop when requested and an
        // image store is wired. A single failed download must not fail the
        // refresh.
        let mut image_saved = false;
        if wants_fetch(options.image_refresh_mode) && self.image_store.is_some() {
            let images = tmdb.all_images(kind, tmdb_id).await;
            for image_type in [ImageType::Primary, ImageType::Backdrop] {
                if let Some(url) = images
                    .iter()
                    .find(|img| img.image_type == image_type)
                    .map(|img| img.url.clone())
                {
                    image_saved |= self
                        .save_image_from_url(item_id, &url, image_type, None)
                        .await
                        .is_ok();
                }
            }
        }
        Ok(image_saved)
    }

    /// The person refresh arm — port of `TmdbPersonProvider` +
    /// `TmdbPersonImageProvider`: resolve the person's TMDB id (stored id,
    /// else the first name-search hit), apply biography/birth/death/birthplace,
    /// and download the profile image. Returns whether the image was saved.
    async fn refresh_person(
        &self,
        tmdb: &Arc<TmdbClient>,
        entity: &mut BaseItemEntity,
        item_id: Uuid,
        name: &str,
        tmdb_id: Option<i64>,
        options: &MetadataRefreshOptions,
    ) -> Result<bool, ServiceError> {
        let tmdb_id = if let Some(id) = tmdb_id {
            id
        } else {
            let Some(hit) = tmdb.search_person(name).await.into_iter().next() else {
                return Ok(false);
            };
            hit.tmdb_id
        };
        if wants_fetch(options.metadata_refresh_mode) {
            if let Some(details) = tmdb.person_details(tmdb_id).await {
                let replace = options.replace_all_metadata;
                set_text(&mut entity.overview, details.biography.as_deref(), replace);
                set_text(
                    &mut entity.production_locations,
                    details.place_of_birth.as_deref(),
                    replace,
                );
                if let Some(date) = details.birthday.as_deref().and_then(parse_ymd)
                    && (replace || entity.premiere_date.is_none())
                {
                    entity.premiere_date = Some(date);
                }
                if let Some(date) = details.deathday.as_deref().and_then(parse_ymd)
                    && (replace || entity.end_date.is_none())
                {
                    entity.end_date = Some(date);
                }
            }
            self.persist_refreshed(entity).await?;
            if let Some(store) = &self.image_store {
                store
                    .save_provider_id(item_id, "Tmdb", &tmdb_id.to_string())
                    .await?;
            }
        }
        if wants_fetch(options.image_refresh_mode)
            && self.image_store.is_some()
            && let Some(url) = tmdb
                .person_lookup(tmdb_id)
                .await
                .and_then(|p| p.profile_url)
        {
            return Ok(self
                .save_image_from_url(item_id, &url, ImageType::Primary, None)
                .await
                .is_ok());
        }
        Ok(false)
    }

    /// The refresh behind [`refresh_full_item`](ProviderManager::refresh_full_item)
    /// / [`refresh_single_item`](ProviderManager::refresh_single_item): resolves
    /// the item's provider record (its stored/chosen ids first, then a title
    /// search), applies the fetched metadata and downloads its artwork per the
    /// refresh modes, and reports what changed.
    async fn refresh_item(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        let (Some(tmdb), Some(items)) = (&self.tmdb, &self.items) else {
            // No remote provider configured — nothing to fetch (faithful: Jellyfin
            // with no metadata plugins leaves the item unchanged).
            return Ok(ItemUpdateType::None);
        };
        let Some(mut entity) = items.retrieve_item(item_id).await? else {
            return Err(ServiceError::not_found(format!("item {item_id}")));
        };
        // ── The one gate (C# `ProviderManager.CanRefreshMetadata` /
        // `CanRefreshImages`, v10.11.8 `MediaBrowser.Providers/Manager/
        // ProviderManager.cs`) ───────────────────────────────────────────────
        // Both the scan and this on-demand path must honour the owning
        // library's fetcher checkboxes and the item's `IsLocked` flag. The scan
        // already did; this path did not, so "Refresh metadata" in the
        // dashboard downloaded TMDB metadata + artwork into a library whose
        // "Metadata downloaders" and "Image fetchers" boxes were all cleared.
        let options = &self.gated_options(&entity, options).await;
        if !wants_fetch(options.metadata_refresh_mode) && !wants_fetch(options.image_refresh_mode) {
            return Ok(ItemUpdateType::None);
        }
        // The verdict (`ItemUpdateType`) is judged against this snapshot.
        let before = entity.clone();
        // "Identify → Apply": the chosen result's ids become the item's
        // (`item.ProviderIds = searchResult.ProviderIds`, persisted because
        // the refresh never erases provider ids), and its name/year are the
        // lookup's (`MetadataService.ApplySearchResult`).
        let provider_ids: Vec<(String, String)> = match &options.search_result {
            Some(result) => {
                let ids: Vec<(String, String)> = result
                    .provider_ids
                    .iter()
                    .flat_map(|ids| ids.iter())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if let Some(store) = &self.image_store {
                    store.replace_provider_ids(item_id, &ids).await?;
                }
                ids
            }
            None => self.stored_provider_ids(item_id).await,
        };
        let chosen = options.search_result.as_ref();
        let chosen_name = chosen
            .and_then(|r| r.name.as_deref())
            .map(str::trim)
            .filter(|n| !n.is_empty());
        let stored_tmdb_id =
            provider_id_of_pairs(&provider_ids, "Tmdb").and_then(|id| id.parse::<i64>().ok());
        // Resolve what to fetch: movies/series by their stored/chosen TMDB id
        // (an IMDb/TVDB id resolves through TMDB's `/find`), else by title;
        // seasons/episodes via their parent series. Music/other kinds have no
        // provider here — the faithful skip.
        let Some(target) = self.resolve_refresh_target(items, &entity).await? else {
            return Ok(ItemUpdateType::None);
        };
        let image_saved = match target {
            RefreshTarget::Title { kind, name, year } => {
                let tmdb_id = if let Some(id) = resolve_tmdb_id(tmdb, kind, &provider_ids).await {
                    id
                } else {
                    let name = chosen_name.unwrap_or(&name);
                    let year = chosen.and_then(|r| r.production_year).or(year);
                    let Some(hit) = tmdb.search(kind, name, year).await.into_iter().next() else {
                        return Ok(ItemUpdateType::None);
                    };
                    hit.tmdb_id
                };
                self.refresh_title(tmdb, &mut entity, item_id, kind, tmdb_id, options)
                    .await?
            }
            RefreshTarget::BoxSet { name } => {
                let name = chosen_name.unwrap_or(&name);
                self.refresh_box_set(tmdb, &mut entity, item_id, name, stored_tmdb_id, options)
                    .await?
            }
            RefreshTarget::Person { name } => {
                let name = chosen_name.unwrap_or(&name);
                self.refresh_person(tmdb, &mut entity, item_id, name, stored_tmdb_id, options)
                    .await?
            }
            tv @ (RefreshTarget::Season { .. } | RefreshTarget::Episode { .. }) => {
                self.refresh_tv(tmdb, &mut entity, item_id, tv, options)
                    .await?
            }
        };
        // What changed: the row's metadata (beyond the refresh stamp) beats an
        // image save, which beats nothing — the closest single value to the C#
        // `ItemUpdateType` flags.
        let metadata_changed = {
            let mut after = entity.clone();
            after.date_last_refreshed = before.date_last_refreshed;
            after != before
        };
        Ok(if metadata_changed {
            ItemUpdateType::MetadataDownload
        } else if image_saved {
            ItemUpdateType::ImageUpdate
        } else {
            ItemUpdateType::None
        })
    }

    /// The error for an image operation on a manager built without the
    /// item-image store / TMDB client it needs (unit tests, hosts that never
    /// wired them) — a configuration error naming the op.
    fn unwired(op: &str) -> ServiceError {
        ServiceError::backend(format!(
            "{op} needs the item-image store and TMDB client, which this provider manager was built without"
        ))
    }

    /// Merges `incoming` into `result_list`, deduplicating by shared provider id.
    ///
    /// Port of the inner merge in C# `GetRemoteSearchResults`: a result matches an
    /// existing one when they agree on the value of any provider-id key. On a
    /// match the existing entry absorbs any provider ids it is missing and adopts
    /// the incoming image url when it had none; otherwise the incoming result is
    /// appended.
    fn merge_search_result(
        result_list: &mut Vec<RemoteSearchResult>,
        incoming: RemoteSearchResult,
    ) {
        let incoming_ids = incoming.provider_ids.clone().unwrap_or_default();

        let existing = result_list.iter_mut().find(|existing| {
            let existing_ids = existing.provider_ids.as_ref();
            incoming_ids.iter().any(|(key, value)| {
                existing_ids
                    .and_then(|ids| ids.get(key))
                    .is_some_and(|existing_value| existing_value.eq_ignore_ascii_case(value))
            })
        });

        match existing {
            Some(existing) => {
                let ids = existing.provider_ids.get_or_insert_with(Default::default);
                for (key, value) in incoming_ids {
                    ids.entry(key).or_insert(value);
                }
                if existing
                    .image_url
                    .as_deref()
                    .map(str::trim)
                    .is_none_or(str::is_empty)
                {
                    existing.image_url = incoming.image_url;
                }
            }
            None => result_list.push(incoming),
        }
    }
}

/// What a metadata refresh should fetch for one item — resolved from the item's
/// kind and (for seasons/episodes) its parent-series linkage.
enum RefreshTarget {
    /// A movie or series: TMDB-search by its own title.
    Title {
        /// The TMDB search kind (movie vs series).
        kind: TmdbKind,
        /// The title to search for.
        name: String,
        /// The production year narrowing the search, when known.
        year: Option<i32>,
    },
    /// A season: search the parent series, then fetch `/tv/{id}/season/{n}`.
    Season {
        /// The parent series row id (its stored provider ids pin the series).
        series_id: Option<Uuid>,
        /// The parent series title driving the TMDB search.
        series_name: String,
        /// The parent series year, when known.
        series_year: Option<i32>,
        /// The season number within the series.
        season_number: i32,
    },
    /// A box set: search TMDB's *collections* by name, then fetch
    /// `/collection/{id}` (C# `TmdbBoxSetProvider`).
    BoxSet {
        /// The collection title to search for.
        name: String,
    },
    /// A person: TMDB's `/search/person` by name, then `/person/{id}`
    /// (C# `TmdbPersonProvider` + `TmdbPersonImageProvider`).
    Person {
        /// The person's name to search for.
        name: String,
    },
    /// An episode: like a season, then select the episode within it.
    Episode {
        /// The parent series row id (its stored provider ids pin the series).
        series_id: Option<Uuid>,
        /// The parent series title driving the TMDB search.
        series_name: String,
        /// The parent series year, when known.
        series_year: Option<i32>,
        /// The season number within the series.
        season_number: i32,
        /// The episode number within the season.
        episode_number: i32,
    },
}

/// One remote image provider as the "Choose Image" flow sees it: the
/// `IRemoteImageProvider` implementations Jellyfin registers, keyed by the
/// client Ferrofin wires for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteImageSource {
    /// `TmdbMovieImageProvider` / `TmdbSeriesImageProvider`: posters,
    /// backdrops (languaged → thumbs) and logos for a title.
    TmdbTitle(TmdbKind),
    /// `TmdbBoxSetImageProvider`: a collection's poster + backdrop.
    TmdbBoxSet,
    /// `TmdbPersonImageProvider`: a person's profile image.
    TmdbPerson,
    /// The fanart.tv plugin's `MovieProvider`.
    FanartMovie,
    /// The fanart.tv plugin's `SeriesProvider`.
    FanartSeries,
    /// The fanart.tv plugin's `ArtistProvider`.
    FanartArtist,
    /// The fanart.tv plugin's `AlbumProvider`.
    FanartAlbum,
    /// `AudioDbArtistImageProvider`.
    AudioDbArtist,
    /// `AudioDbAlbumImageProvider`.
    AudioDbAlbum,
    /// `OmdbImageProvider`: the poster of a movie/trailer/episode.
    Omdb,
    /// The Studio Images artwork repository: a studio's thumb.
    Studios,
}

impl RemoteImageSource {
    /// The provider's display name (`IRemoteImageProvider.Name`).
    fn name(self) -> &'static str {
        match self {
            Self::TmdbTitle(_) | Self::TmdbBoxSet | Self::TmdbPerson => TMDB_PROVIDER_NAME,
            Self::FanartMovie | Self::FanartSeries | Self::FanartArtist | Self::FanartAlbum => {
                FANART_PROVIDER_NAME
            }
            Self::AudioDbArtist | Self::AudioDbAlbum => AUDIODB_PROVIDER_NAME,
            Self::Omdb => OMDB_PROVIDER_NAME,
            Self::Studios => crate::studios::PROVIDER_NAME,
        }
    }

    /// The image types the provider can return (`GetSupportedImages`) — each
    /// list verbatim from the C# provider.
    fn supported_images(self) -> &'static [ImageType] {
        use ImageType::{Art, Backdrop, Banner, Disc, Logo, Primary, Thumb};
        match self {
            Self::TmdbTitle(_) => &[Primary, Backdrop, Logo, Thumb],
            Self::TmdbBoxSet => &[Primary, Backdrop, Thumb],
            Self::TmdbPerson | Self::Omdb => &[Primary],
            Self::FanartMovie => &[Primary, Thumb, Art, Logo, Disc, Banner, Backdrop],
            Self::FanartSeries => &[Primary, Thumb, Art, Logo, Backdrop, Banner],
            Self::FanartArtist => &[Primary, Logo, Art, Banner, Backdrop],
            Self::FanartAlbum | Self::AudioDbAlbum => &[Primary, Disc],
            Self::AudioDbArtist => &[Primary, Logo, Banner, Backdrop],
            Self::Studios => &[Thumb],
        }
    }
}

impl LocalProviderManager {
    /// The remote image providers that `Supports(item)` for `entity`'s kind,
    /// in registration order, restricted to the clients actually wired.
    fn image_sources_for(&self, entity: &BaseItemEntity) -> Vec<RemoteImageSource> {
        let mut sources = Vec::new();
        let has = |flag: bool, source: RemoteImageSource| flag.then_some(source);
        match short_kind(entity) {
            "Movie" | "Trailer" => {
                sources.extend(has(
                    self.tmdb.is_some(),
                    RemoteImageSource::TmdbTitle(TmdbKind::Movie),
                ));
                sources.extend(has(self.fanart.is_some(), RemoteImageSource::FanartMovie));
                sources.extend(has(self.omdb.is_some(), RemoteImageSource::Omdb));
            }
            "Series" => {
                sources.extend(has(
                    self.tmdb.is_some(),
                    RemoteImageSource::TmdbTitle(TmdbKind::Series),
                ));
                sources.extend(has(self.fanart.is_some(), RemoteImageSource::FanartSeries));
            }
            "Episode" => sources.extend(has(self.omdb.is_some(), RemoteImageSource::Omdb)),
            "BoxSet" => sources.extend(has(self.tmdb.is_some(), RemoteImageSource::TmdbBoxSet)),
            "Person" => sources.extend(has(self.tmdb.is_some(), RemoteImageSource::TmdbPerson)),
            "MusicArtist" => {
                sources.extend(has(
                    self.audiodb.is_some(),
                    RemoteImageSource::AudioDbArtist,
                ));
                sources.extend(has(self.fanart.is_some(), RemoteImageSource::FanartArtist));
            }
            "MusicAlbum" => {
                sources.extend(has(self.audiodb.is_some(), RemoteImageSource::AudioDbAlbum));
                sources.extend(has(self.fanart.is_some(), RemoteImageSource::FanartAlbum));
            }
            "Studio" => sources.extend(has(self.studios.is_some(), RemoteImageSource::Studios)),
            _ => {}
        }
        sources
    }

    /// Asks one provider for `entity`'s images (`IRemoteImageProvider.GetImages`),
    /// keyed by the item's stored external ids `ids`. Empty when the id the
    /// provider needs is absent or the remote has nothing.
    async fn images_from(
        &self,
        source: RemoteImageSource,
        entity: &BaseItemEntity,
        ids: &[(String, String)],
    ) -> Vec<TmdbImage> {
        match source {
            RemoteImageSource::TmdbTitle(_)
            | RemoteImageSource::TmdbBoxSet
            | RemoteImageSource::TmdbPerson => self.tmdb_images(source, entity, ids).await,
            RemoteImageSource::FanartMovie
            | RemoteImageSource::FanartSeries
            | RemoteImageSource::FanartArtist
            | RemoteImageSource::FanartAlbum => self.fanart_images(source, ids).await,
            RemoteImageSource::AudioDbArtist
            | RemoteImageSource::AudioDbAlbum
            | RemoteImageSource::Omdb
            | RemoteImageSource::Studios => self.keyed_images(source, entity, ids).await,
        }
    }

    /// The TMDB image providers: a title's poster/backdrop/logo set (by its
    /// stored/found id, else a title search), a box set's collection artwork,
    /// a person's profile.
    async fn tmdb_images(
        &self,
        source: RemoteImageSource,
        entity: &BaseItemEntity,
        ids: &[(String, String)],
    ) -> Vec<TmdbImage> {
        let Some(tmdb) = &self.tmdb else {
            return Vec::new();
        };
        let name = entity_name(entity);
        let stored_tmdb_id =
            provider_id_of_pairs(ids, "Tmdb").and_then(|id| id.parse::<i64>().ok());
        match source {
            RemoteImageSource::TmdbTitle(kind) => {
                // The stored Tmdb id (or an Imdb/Tvdb id via `/find`) pins the
                // title; without any, best-match the title by name/year.
                let mut tmdb_id = resolve_tmdb_id(tmdb, kind, ids).await;
                if tmdb_id.is_none()
                    && let Some((kind, name, year)) = title_lookup(entity)
                {
                    tmdb_id = tmdb
                        .search(kind, &name, year)
                        .await
                        .into_iter()
                        .next()
                        .map(|h| h.tmdb_id);
                }
                match tmdb_id {
                    Some(id) => tmdb.all_images(kind, id).await,
                    None => Vec::new(),
                }
            }
            RemoteImageSource::TmdbBoxSet => {
                // `TmdbBoxSetImageProvider`: the collection's artwork, by the
                // box set's Tmdb id or its name.
                let mut collection_id = stored_tmdb_id;
                if collection_id.is_none()
                    && let Some(name) = name
                {
                    collection_id = tmdb
                        .search_collection(name)
                        .await
                        .into_iter()
                        .next()
                        .map(|h| h.tmdb_id);
                }
                let Some(collection_id) = collection_id else {
                    return Vec::new();
                };
                tmdb.collection(collection_id)
                    .await
                    .map(|c| {
                        c.images
                            .into_iter()
                            .map(|img| plain_image(img.image_type, img.url))
                            .collect()
                    })
                    .unwrap_or_default()
            }
            _ => {
                // `TmdbPersonImageProvider`: the person's profile by Tmdb id,
                // else the first name-search hit.
                let person = match (stored_tmdb_id, name) {
                    (Some(id), _) => tmdb.person_lookup(id).await,
                    (None, Some(name)) => tmdb.search_person(name).await.into_iter().next(),
                    (None, None) => None,
                };
                person
                    .and_then(|p| p.profile_url)
                    .map(|url| vec![plain_image(ImageType::Primary, url)])
                    .unwrap_or_default()
            }
        }
    }

    /// The fanart.tv image providers, each keyed by the id the plugin reads:
    /// movies by Tmdb (else Imdb), series by Tvdb, artists by MusicBrainz
    /// artist, albums by album-artist + release-group.
    async fn fanart_images(
        &self,
        source: RemoteImageSource,
        ids: &[(String, String)],
    ) -> Vec<TmdbImage> {
        let Some(fanart) = &self.fanart else {
            return Vec::new();
        };
        let id = |key: &str| provider_id_of_pairs(ids, key);
        match source {
            RemoteImageSource::FanartMovie => match id("Tmdb").or_else(|| id("Imdb")) {
                Some(movie) => fanart.movie_images(movie).await,
                None => Vec::new(),
            },
            RemoteImageSource::FanartSeries => match id("Tvdb") {
                Some(tvdb) => fanart.series_images(tvdb).await,
                None => Vec::new(),
            },
            RemoteImageSource::FanartArtist => match id("MusicBrainzArtist") {
                Some(artist) => fanart.artist_images(artist).await,
                None => Vec::new(),
            },
            _ => match (id("MusicBrainzAlbumArtist"), id("MusicBrainzReleaseGroup")) {
                (Some(artist), Some(group)) => fanart.album_images(artist, group).await,
                _ => Vec::new(),
            },
        }
    }

    /// The single-key image providers: TheAudioDb (artist/album by MusicBrainz
    /// id), OMDb's poster (by IMDb id), the studio artwork repository (by name).
    async fn keyed_images(
        &self,
        source: RemoteImageSource,
        entity: &BaseItemEntity,
        ids: &[(String, String)],
    ) -> Vec<TmdbImage> {
        let id = |key: &str| provider_id_of_pairs(ids, key);
        match source {
            RemoteImageSource::AudioDbArtist => match (&self.audiodb, id("MusicBrainzArtist")) {
                (Some(audiodb), Some(artist)) => audiodb
                    .artist(artist)
                    .await
                    .map(|a| a.images)
                    .unwrap_or_default(),
                _ => Vec::new(),
            },
            RemoteImageSource::AudioDbAlbum => {
                match (&self.audiodb, id("MusicBrainzReleaseGroup")) {
                    (Some(audiodb), Some(group)) => audiodb
                        .album(group)
                        .await
                        .map(|a| a.images)
                        .unwrap_or_default(),
                    _ => Vec::new(),
                }
            }
            RemoteImageSource::Omdb => match (&self.omdb, id("Imdb")) {
                (Some(omdb), Some(imdb)) => omdb
                    .item(imdb)
                    .await
                    .and_then(|item| item.poster)
                    .map(|url| vec![plain_image(ImageType::Primary, url)])
                    .unwrap_or_default(),
                _ => Vec::new(),
            },
            _ => match (&self.studios, entity_name(entity)) {
                (Some(studios), Some(name)) => studios
                    .thumb_url(name)
                    .await
                    .map(|url| vec![plain_image(ImageType::Thumb, url)])
                    .unwrap_or_default(),
                _ => Vec::new(),
            },
        }
    }
}

/// The row's trimmed, non-empty name.
fn entity_name(entity: &BaseItemEntity) -> Option<&str> {
    entity
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
}

/// A type + URL image with no size/rating/language metadata.
fn plain_image(image_type: ImageType, url: String) -> TmdbImage {
    TmdbImage {
        image_type,
        url,
        width: None,
        height: None,
        community_rating: None,
        vote_count: None,
        language: None,
    }
}

/// A case-insensitive, non-empty lookup in a `(key, value)` id list (the
/// stored `BaseItemProviders` shape).
fn provider_id_of_pairs<'a>(ids: &'a [(String, String)], key: &str) -> Option<&'a str> {
    ids.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

/// The TMDB id an item's external ids pin it to: its own `Tmdb` id, else an
/// `Imdb` id (or, for series, a `Tvdb` id) resolved through TMDB's `/find` —
/// the `TmdbMovieProvider`/`TmdbSeriesProvider.GetMetadata` precedence. `None`
/// when nothing on the item identifies it, so the caller searches by title.
async fn resolve_tmdb_id(
    tmdb: &Arc<TmdbClient>,
    kind: TmdbKind,
    ids: &[(String, String)],
) -> Option<i64> {
    if let Some(id) = provider_id_of_pairs(ids, "Tmdb").and_then(|v| v.parse::<i64>().ok()) {
        return Some(id);
    }
    if let Some(imdb) = provider_id_of_pairs(ids, "Imdb")
        && let Some(id) = tmdb.find_by_external_id(kind, "imdb_id", imdb).await
    {
        return Some(id);
    }
    if kind == TmdbKind::Series
        && let Some(tvdb) = provider_id_of_pairs(ids, "Tvdb")
        && let Some(id) = tmdb.find_by_external_id(kind, "tvdb_id", tvdb).await
    {
        return Some(id);
    }
    None
}

/// The short (C#-unqualified) kind name of a row, e.g. the stored
/// `MediaBrowser.Controller.Entities.TV.Episode` → `Episode`.
fn short_kind(entity: &BaseItemEntity) -> &str {
    entity.type_.rsplit('.').next().unwrap_or(&entity.type_)
}

/// The pure `(kind, name, year)` extraction for a movie/series row, shared by
/// [`LocalProviderManager::get_available_remote_images`] and the refresh-target
/// resolver.
fn title_lookup(entity: &BaseItemEntity) -> Option<(TmdbKind, String, Option<i32>)> {
    let kind = match short_kind(entity) {
        "Movie" => TmdbKind::Movie,
        "Series" => TmdbKind::Series,
        _ => return None,
    };
    let name = entity.name.clone().filter(|n| !n.is_empty())?;
    let year = entity.production_year.and_then(|y| i32::try_from(y).ok());
    Some((kind, name, year))
}

/// Classifies what a refresh should fetch for `entity` — the pure half of
/// [`LocalProviderManager::resolve_refresh_target`]. `series` is the item's
/// parent-series row when one was resolvable (seasons/episodes only). `None`
/// for kinds with no provider (music etc.), broken series links, or missing
/// season/episode numbers.
fn refresh_target_of(
    entity: &BaseItemEntity,
    series: Option<&BaseItemEntity>,
) -> Option<RefreshTarget> {
    let kind = short_kind(entity);
    if matches!(kind, "Movie" | "Series") {
        return title_lookup(entity).map(|(kind, name, year)| RefreshTarget::Title {
            kind,
            name,
            year,
        });
    }
    if kind == "BoxSet" {
        return entity
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .map(|name| RefreshTarget::BoxSet { name });
    }
    if kind == "Person" {
        return entity
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .map(|name| RefreshTarget::Person { name });
    }
    if !matches!(kind, "Season" | "Episode") {
        return None;
    }
    let series = series?;
    let series_id = Uuid::parse_str(&series.id).ok();
    let series_name = series.name.clone().filter(|n| !n.is_empty())?;
    let series_year = series.production_year.and_then(|y| i32::try_from(y).ok());
    let number = |v: Option<i64>| v.and_then(|n| i32::try_from(n).ok());
    if kind == "Season" {
        Some(RefreshTarget::Season {
            series_id,
            series_name,
            series_year,
            season_number: number(entity.index_number)?,
        })
    } else {
        // Episode: `parent_index_number` is the season, `index_number` the
        // episode within it.
        Some(RefreshTarget::Episode {
            series_id,
            series_name,
            series_year,
            season_number: number(entity.parent_index_number)?,
            episode_number: number(entity.index_number)?,
        })
    }
}

/// Fills or replaces an item row's name + overview (the season/episode TMDB
/// fields), with the same fill-or-replace semantics as [`apply_tmdb_details`].
fn apply_name_overview(
    entity: &mut BaseItemEntity,
    name: Option<&str>,
    overview: Option<&str>,
    replace: bool,
) {
    set_text(&mut entity.name, name, replace);
    set_text(&mut entity.overview, overview, replace);
}

/// Whether a [`MetadataRefreshMode`] should fetch remote data (`Default` /
/// `FullRefresh`); `None` / `ValidationOnly` skip the network.
fn wants_fetch(mode: MetadataRefreshMode) -> bool {
    matches!(
        mode,
        MetadataRefreshMode::Default | MetadataRefreshMode::FullRefresh
    )
}

/// Copies `new` into `cur` when `new` has a value and either `replace` is set or
/// `cur` is currently empty. Mirrors the scanner's fill-or-replace merge.
fn set_text(cur: &mut Option<String>, new: Option<&str>, replace: bool) {
    if let Some(value) = new
        && (replace || cur.as_deref().is_none_or(str::is_empty))
    {
        *cur = Some(value.to_owned());
    }
}

/// Applies fetched TMDB [`TmdbDetails`] onto an item row. Each field is filled
/// when empty, or overwritten when `replace` is set (the C# `FullRefresh`
/// `ReplaceAllMetadata` behavior); a field TMDB did not return is left untouched.
/// Mirrors the scanner's `apply_details` merge.
fn apply_tmdb_details(entity: &mut BaseItemEntity, details: &TmdbDetails, replace: bool) {
    // `ProviderUtils.MergeBaseItemData`: the name is replaced only on
    // `replaceData` (or when empty); the sort key follows the name
    // (`BaseItem.CreateSortName`) unless a `ForcedSortName` pins it.
    let before = entity.name.clone();
    set_text(&mut entity.name, details.name.as_deref(), replace);
    if entity.name != before && entity.forced_sort_name.is_none() {
        entity.sort_name = entity
            .name
            .as_deref()
            .map(ferrofin_util::sort_name::create_sort_name);
    }
    set_text(
        &mut entity.original_title,
        details.original_title.as_deref(),
        replace,
    );
    set_text(&mut entity.overview, details.overview.as_deref(), replace);
    set_text(&mut entity.tagline, details.tagline.as_deref(), replace);
    set_text(
        &mut entity.official_rating,
        details.official_rating.as_deref(),
        replace,
    );
    if details.community_rating.is_some() && (replace || entity.community_rating.is_none()) {
        entity.community_rating = details.community_rating;
    }
    if !details.genres.is_empty()
        && (replace || entity.genres.as_deref().unwrap_or_default().is_empty())
    {
        entity.genres = Some(details.genres.join("|"));
    }
    if !details.studios.is_empty()
        && (replace || entity.studios.as_deref().unwrap_or_default().is_empty())
    {
        entity.studios = Some(details.studios.join("|"));
    }
    if details.production_year.is_some() && (replace || entity.production_year.is_none()) {
        entity.production_year = details.production_year.map(i64::from);
    }
    if let Some(mins) = details.runtime_minutes
        && (replace || entity.run_time_ticks.is_none())
    {
        // Ticks are 100-ns units: minutes × 60 s × 10,000,000.
        entity.run_time_ticks = Some(i64::from(mins) * 600_000_000);
    }
    if let Some(date) = details.premiere_date.as_deref().and_then(parse_ymd)
        && (replace || entity.premiere_date.is_none())
    {
        entity.premiere_date = Some(date);
    }
}

/// Parses a TMDB `YYYY-MM-DD` date into a UTC midnight timestamp.
fn parse_ymd(s: &str) -> Option<chrono::DateTime<Utc>> {
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    Some(chrono::DateTime::from_naive_utc_and_offset(
        date.and_hms_opt(0, 0, 0)?,
        Utc,
    ))
}

/// The on-disk filename stem for an uploaded image of `image_type`/`index`,
/// e.g. `Primary` → `primary`, `Backdrop` index 2 → `backdrop2`.
fn image_file_stem(image_type: ImageType, index: Option<i32>) -> String {
    let base = format!("{image_type:?}").to_ascii_lowercase();
    match index.filter(|&i| i > 0) {
        Some(i) => format!("{base}{i}"),
        None => base,
    }
}

#[async_trait]
impl ProviderManager for LocalProviderManager {
    async fn queue_refresh(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
        _priority: RefreshPriority,
    ) -> Result<(), ServiceError> {
        // Run the refresh in the background and return immediately — the C#
        // queued-refresh shape (the refresh button 204s while the fetch runs).
        // ponytail: _priority ignored — a single-item spawn needs no ordering.
        // Add an ordered, priority-aware worker queue when bulk/library-wide
        // refreshes land.
        let mgr = self.clone();
        let options = options.clone();
        let handle = tokio::spawn(async move {
            if let Err(err) = mgr.refresh_full_item(item_id, &options).await {
                tracing::warn!(%item_id, %err, "queued metadata refresh failed");
            }
        });
        drop(handle);
        Ok(())
    }

    async fn refresh_full_item(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        self.refresh_item(item_id, options).await.map(|_| ())
    }

    async fn refresh_single_item(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        // `ProviderManager.RefreshSingleItem`: the item's own metadata service
        // runs and reports what changed. Ferrofin's refresh never recurses
        // into children, so this is the full refresh with its verdict kept.
        self.refresh_item(item_id, options).await
    }

    async fn save_image_from_url(
        &self,
        item_id: Uuid,
        url: &str,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        let Some(tmdb) = &self.tmdb else {
            return Err(Self::unwired("save_image_from_url"));
        };
        let bytes = tmdb.download(url).await.ok_or_else(|| {
            ServiceError::backend(format!("could not download remote image {url}"))
        })?;
        // Reuse the local write+persist path.
        let mime = if url.to_ascii_lowercase().ends_with(".png") {
            "image/png"
        } else {
            "image/jpeg"
        };
        self.save_image(item_id, &bytes, mime, image_type, image_index)
            .await
    }

    async fn save_image(
        &self,
        item_id: Uuid,
        content: &[u8],
        mime_type: &str,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        let (Some(store), Some(meta_root)) = (&self.image_store, &self.metadata_dir) else {
            return Err(Self::unwired("save_image"));
        };
        let ext = mime_types::to_extension(&mime_type.to_ascii_lowercase())
            .unwrap_or(".jpg")
            .trim_start_matches('.');
        // The scan's art dir for this item (`{meta}/library/{db-format id}`,
        // same stem naming): writing the upload there makes it the file the
        // scan re-discovers on every rescan, so an uploaded image replaces the
        // downloaded artwork durably instead of being wiped by the next scan's
        // image rewrite.
        let item_dir = meta_root.join(ferrofin_db::store::guid_to_db(item_id));
        let stem = image_file_stem(image_type, image_index);
        let dest = item_dir.join(format!("{stem}.{ext}"));
        std::fs::create_dir_all(&item_dir).map_err(|e| {
            ProvidersError::io(format!("create image dir {}", item_dir.display()), e)
        })?;
        // Purge same-stem files of other extensions (e.g. the scan's cached
        // `primary.jpg` when a PNG is uploaded) so this upload is the single
        // candidate the scan finds for the stem.
        for other in ["jpg", "jpeg", "png", "webp", "gif"] {
            if other != ext {
                let _ = std::fs::remove_file(item_dir.join(format!("{stem}.{other}")));
            }
        }
        std::fs::write(&dest, content)
            .map_err(|e| ProvidersError::io(format!("write image {}", dest.display()), e))?;
        store
            .set_item_image(
                item_id,
                &ItemImageInfo {
                    path: dest.to_string_lossy().into_owned(),
                    image_type,
                    date_modified: Utc::now(),
                    width: 0,
                    height: 0,
                    blur_hash: None,
                },
            )
            .await
    }

    async fn delete_image(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        let Some(store) = &self.image_store else {
            return Err(Self::unwired("delete_image"));
        };
        // Remove the DB rows, then the files they pointed at (best-effort — a
        // missing file must not fail the delete).
        let paths = store
            .delete_item_image(item_id, image_type, image_index)
            .await?;
        for path in paths {
            let _ = std::fs::remove_file(&path);
        }
        Ok(())
    }

    async fn get_available_remote_images(
        &self,
        item_id: Uuid,
        query: &RemoteImageQuery,
    ) -> Result<Vec<RemoteImageInfo>, ServiceError> {
        let Some(items) = &self.items else {
            return Ok(Vec::new());
        };
        let Some(entity) = items.retrieve_item(item_id).await? else {
            return Ok(Vec::new());
        };
        // `ProviderManager.GetAvailableRemoteImages`: every remote image
        // provider for the item, narrowed to `ProviderName` when given
        // (`OrdinalIgnoreCase`), each asked for its images, which are then
        // narrowed to the requested `ImageType`.
        let name_filter = Some(query.provider_name.trim()).filter(|n| !n.is_empty());
        let sources: Vec<RemoteImageSource> = self
            .image_sources_for(&entity)
            .into_iter()
            .filter(|source| name_filter.is_none_or(|n| source.name().eq_ignore_ascii_case(n)))
            .collect();
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        let ids = self.stored_provider_ids(item_id).await;
        let mut results = Vec::new();
        for source in sources {
            let images = self.images_from(source, &entity, &ids).await;
            results.extend(
                images
                    .into_iter()
                    .filter(|img| query.image_type.is_none_or(|t| t == img.image_type))
                    .map(|img| RemoteImageInfo {
                        provider_name: Some(source.name().to_owned()),
                        url: Some(img.url),
                        width: img.width,
                        height: img.height,
                        community_rating: img.community_rating,
                        vote_count: img.vote_count,
                        language: img.language,
                        type_: img.image_type,
                        ..RemoteImageInfo::default()
                    }),
            );
        }
        Ok(results)
    }

    async fn get_remote_image_provider_info(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ImageProviderInfo>, ServiceError> {
        let Some(items) = &self.items else {
            return Ok(Vec::new());
        };
        let Some(entity) = items.retrieve_item(item_id).await? else {
            return Ok(Vec::new());
        };
        // `GetRemoteImageProviderInfo`: each provider's name + the image types
        // its `GetSupportedImages(item)` reports.
        Ok(self
            .image_sources_for(&entity)
            .into_iter()
            .map(|source| ImageProviderInfo {
                name: Some(source.name().to_owned()),
                supported_images: source.supported_images().to_vec(),
            })
            .collect())
    }

    async fn save_metadata(
        &self,
        _item_id: Uuid,
        _update_type: ItemUpdateType,
    ) -> Result<(), ServiceError> {
        // The DB-side persistence already happened in the caller (e.g.
        // `save_image_from_url` → `set_item_image`). The remaining C# work is
        // writing metadata savers (NFO/images to the media folder), which Ferrofin
        // defers — so this is a successful no-op rather than a hard failure, and
        // the image-download / metadata-edit flows complete.
        Ok(())
    }

    async fn get_external_id_infos(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ExternalIdInfo>, ServiceError> {
        // C# filters every registered `IExternalId` by `Supports(item)`, so the
        // Identify dialog offers only the id fields that apply to the item's
        // type. That needs the item; without a store (or the type table) fall
        // back to the descriptors supplied at construction.
        let mut infos = Vec::new();
        if let (Some(items), false) = (self.items.as_ref(), self.kind_by_type_name.is_empty())
            && let Some(item) = items.retrieve_item(item_id).await?
            && let Some(kind) = self.kind_by_type_name.get(item.type_.as_str())
        {
            infos = crate::external_ids::external_id_infos(*kind);
        }
        // Supplied descriptors are *extra* providers (a host registering its
        // own), so skip any that the kind-filtered set already advertises.
        let extras: Vec<ExternalIdInfo> = self
            .external_id_infos
            .iter()
            .filter(|extra| {
                !infos
                    .iter()
                    .any(|known| known.key == extra.key && known.type_ == extra.type_)
            })
            .cloned()
            .collect();
        infos.extend(extras);
        Ok(infos)
    }

    async fn remote_search(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        // Select the providers that serve this item kind, then (if a provider
        // name was supplied) narrow to that provider — a port of the C#
        // `GetMetadataProvidersInternal(...).OfType<IRemoteSearchProvider>()`
        // filter chain. With no fetcher registered this set is empty and the
        // loop below yields `[]`, exactly as Jellyfin returns when nothing
        // matches.
        let name_filter = request.search_provider_name.as_deref();
        let providers = self.remote_search_providers.iter().filter(|p| {
            p.supports(request.item_kind)
                && name_filter.is_none_or(|n| p.name().eq_ignore_ascii_case(n))
        });

        let mut result_list: Vec<RemoteSearchResult> = Vec::new();

        for provider in providers {
            // Per-provider failures are logged-and-skipped in C#; here we simply
            // continue so one bad provider can't fail the whole search.
            let Ok(results) = provider.get_search_results(request).await else {
                continue;
            };

            for mut result in results {
                result.search_provider_name = Some(provider.name().to_owned());
                Self::merge_search_result(&mut result_list, result);
            }
        }

        Ok(result_list)
    }

    async fn get_all_metadata_plugins(&self) -> Result<Vec<MetadataPluginSummary>, ServiceError> {
        Ok(crate::library_options::all_metadata_plugins())
    }

    async fn get_library_options_info(
        &self,
        item_types: &[String],
        is_new_library: bool,
    ) -> Result<ferrofin_model::configuration::LibraryOptionsResultDto, ServiceError> {
        Ok(crate::library_options::library_options_info(
            item_types,
            is_new_library,
            &self.dynamic_fetchers,
        ))
    }

    async fn get_metadata_options(&self, _item_id: Uuid) -> Result<MetadataOptions, ServiceError> {
        Ok(MetadataOptions::default())
    }

    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::{
        LocalProviderManager, RemoteSearchProvider, apply_tmdb_details, parse_ymd, set_text,
        wants_fetch,
    };
    use crate::tmdb::TmdbDetails;
    use ferrofin_traits::providers::{MetadataRefreshMode as Mode, MetadataRefreshOptions as Opts};

    use async_trait::async_trait;
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::ImageType;
    use ferrofin_model::providers::{
        ExternalIdInfo, ItemLookupInfo, RemoteImageQuery, RemoteSearchResult,
    };
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::providers::{
        ItemUpdateType, MetadataRefreshMode, MetadataRefreshOptions, ProviderManager,
        RefreshPriority, RemoteSearchRequest,
    };
    use uuid::Uuid;

    /// A synthetic remote-search provider for exercising the merge/dedup port.
    struct FakeProvider {
        name: String,
        kind: BaseItemKind,
        results: Vec<RemoteSearchResult>,
        fail: bool,
    }

    #[async_trait]
    impl RemoteSearchProvider for FakeProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn supports(&self, item_kind: BaseItemKind) -> bool {
            item_kind == self.kind
        }

        async fn get_search_results(
            &self,
            _request: &RemoteSearchRequest,
        ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
            if self.fail {
                return Err(ServiceError::backend("boom"));
            }
            Ok(self.results.clone())
        }
    }

    fn result_with(name: &str, ids: &[(&str, &str)], image: Option<&str>) -> RemoteSearchResult {
        let mut map = HashMap::new();
        for (k, v) in ids {
            map.insert((*k).to_owned(), (*v).to_owned());
        }
        RemoteSearchResult {
            name: Some(name.to_owned()),
            provider_ids: Some(map),
            image_url: image.map(str::to_owned),
            ..RemoteSearchResult::default()
        }
    }

    fn request(kind: BaseItemKind) -> RemoteSearchRequest {
        RemoteSearchRequest {
            item_kind: kind,
            ..RemoteSearchRequest::default()
        }
    }

    /// A request for `kind` searching by `name`.
    fn named_request(kind: BaseItemKind, name: &str) -> RemoteSearchRequest {
        RemoteSearchRequest {
            item_kind: kind,
            search_info: ItemLookupInfo {
                name: Some(name.to_owned()),
                ..ItemLookupInfo::default()
            },
            ..RemoteSearchRequest::default()
        }
    }

    /// A request for `kind` carrying `ids` on the item itself.
    fn id_request(kind: BaseItemKind, ids: &[(&str, &str)]) -> RemoteSearchRequest {
        RemoteSearchRequest {
            item_kind: kind,
            search_info: ItemLookupInfo {
                provider_ids: Some(
                    ids.iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect(),
                ),
                ..ItemLookupInfo::default()
            },
            ..RemoteSearchRequest::default()
        }
    }

    /// Spawns a mock MusicBrainz server + client over `routes`.
    async fn musicbrainz_over(
        routes: Vec<(&'static str, String)>,
    ) -> (
        crate::mock_http::MockServer,
        Arc<crate::musicbrainz::MusicBrainzClient>,
    ) {
        let server = crate::mock_http::MockServer::start(routes).await;
        let client = Arc::new(crate::musicbrainz::MusicBrainzClient::new(
            &server.base_url,
            "test",
        ));
        (server, client)
    }

    const MB_RELEASE_HIT: &str = r#"{"releases":[{"id":"rel-1","title":"Kind of Blue","date":"1959-08-17","release-group":{"id":"rg-1"},"artist-credit":[{"name":"Miles Davis","artist":{"id":"artist-mbid","name":"Miles Davis"}}]}]}"#;

    #[tokio::test]
    async fn musicbrainz_album_identify_searches_by_title_and_artist_name() {
        use super::MusicBrainzAlbumSearchProvider;
        let (_server, mb) =
            musicbrainz_over(vec![("/ws/2/release?", MB_RELEASE_HIT.to_owned())]).await;
        let provider = MusicBrainzAlbumSearchProvider::new(mb);
        assert_eq!(provider.name(), "MusicBrainz");
        assert!(provider.supports(BaseItemKind::MusicAlbum));
        assert!(!provider.supports(BaseItemKind::MusicArtist));

        // The album artist comes from the contained songs first
        // (`AlbumInfoExtensions.GetAlbumArtist`).
        let mut request = named_request(BaseItemKind::MusicAlbum, "Kind \"of\" Blue");
        request.album_artists = vec!["Wrong Artist".to_owned()];
        request.song_infos = vec![ferrofin_model::providers::SongInfo {
            album_artists: vec![String::new(), "Miles Davis".to_owned()],
            ..ferrofin_model::providers::SongInfo::default()
        }];
        let results = provider
            .get_search_results(&request)
            .await
            .expect("results");
        assert_eq!(results.len(), 1);
        let hit = &results[0];
        assert_eq!(hit.name.as_deref(), Some("Kind of Blue"));
        assert_eq!(hit.production_year, Some(1959));
        assert_eq!(
            hit.premiere_date.map(|d| d.to_rfc3339()),
            Some("1959-08-17T00:00:00+00:00".to_owned())
        );
        assert_eq!(hit.search_provider_name.as_deref(), Some("MusicBrainz"));
        let ids = hit.provider_ids.as_ref().expect("ids");
        assert_eq!(ids["MusicBrainzAlbum"], "rel-1");
        assert_eq!(ids["MusicBrainzReleaseGroup"], "rg-1");
        // Artist credits: first is the album artist, each carries its MBID.
        assert_eq!(hit.artists.len(), 1);
        assert_eq!(hit.artists[0].name.as_deref(), Some("Miles Davis"));
        assert_eq!(
            hit.artists[0].provider_ids.as_ref().expect("artist ids")["MusicBrainzArtist"],
            "artist-mbid"
        );
        assert_eq!(
            hit.album_artist.as_deref().and_then(|a| a.name.as_deref()),
            Some("Miles Davis")
        );
    }

    #[tokio::test]
    async fn musicbrainz_album_identify_resolves_a_known_release_id_exactly() {
        use super::MusicBrainzAlbumSearchProvider;
        let (_server, mb) = musicbrainz_over(vec![(
            "/ws/2/release/rel-9",
            r#"{"id":"rel-9","title":"Exact","date":"2001","artist-credit":[]}"#.to_owned(),
        )])
        .await;
        let provider = MusicBrainzAlbumSearchProvider::new(mb);
        // The song's `MusicBrainzAlbum` is the fallback for the album's own.
        let mut request = named_request(BaseItemKind::MusicAlbum, "ignored");
        request.song_infos = vec![ferrofin_model::providers::SongInfo {
            base: ItemLookupInfo {
                provider_ids: Some(HashMap::from([(
                    "musicbrainzalbum".to_owned(),
                    "rel-9".to_owned(),
                )])),
                ..ItemLookupInfo::default()
            },
            ..ferrofin_model::providers::SongInfo::default()
        }];
        let results = provider
            .get_search_results(&request)
            .await
            .expect("results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name.as_deref(), Some("Exact"));
        assert_eq!(results[0].production_year, Some(2001));
        assert!(results[0].artists.is_empty());
        assert!(results[0].album_artist.is_none());
        assert_eq!(
            results[0].provider_ids.as_ref().expect("ids")["MusicBrainzAlbum"],
            "rel-9"
        );
    }

    #[tokio::test]
    async fn musicbrainz_album_identify_expands_a_release_group() {
        use super::MusicBrainzAlbumSearchProvider;
        let (_server, mb) = musicbrainz_over(vec![
            (
                "/ws/2/release-group/rg-7",
                r#"{"releases":[{"id":"rel-a"},{"id":"rel-b"}]}"#.to_owned(),
            ),
            (
                "/ws/2/release/rel-a",
                r#"{"id":"rel-a","title":"A"}"#.to_owned(),
            ),
            (
                "/ws/2/release/rel-b",
                r#"{"id":"rel-b","title":"B"}"#.to_owned(),
            ),
        ])
        .await;
        let provider = MusicBrainzAlbumSearchProvider::new(mb);
        let results = provider
            .get_search_results(&id_request(
                BaseItemKind::MusicAlbum,
                &[("MusicBrainzReleaseGroup", "rg-7")],
            ))
            .await
            .expect("results");
        assert_eq!(
            results
                .iter()
                .filter_map(|r| r.name.as_deref())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );
    }

    #[test]
    fn album_artist_mbid_follows_the_c_sharp_precedence() {
        use super::album_artist_mbid;
        // Own `MusicBrainzAlbumArtist` wins.
        let mut request = id_request(
            BaseItemKind::MusicAlbum,
            &[("MusicBrainzAlbumArtist", "own")],
        );
        request.artist_provider_ids = Some(HashMap::from([(
            "MusicBrainzArtist".to_owned(),
            "artist".to_owned(),
        )]));
        assert_eq!(album_artist_mbid(&request).as_deref(), Some("own"));
        // Then the artist's own id.
        request.search_info.provider_ids = None;
        assert_eq!(album_artist_mbid(&request).as_deref(), Some("artist"));
        // Then the first song's album-artist id; blanks are absent.
        request.artist_provider_ids = Some(HashMap::from([(
            "MusicBrainzArtist".to_owned(),
            "  ".to_owned(),
        )]));
        request.song_infos = vec![ferrofin_model::providers::SongInfo {
            base: ItemLookupInfo {
                provider_ids: Some(HashMap::from([(
                    "MusicBrainzAlbumArtist".to_owned(),
                    "song".to_owned(),
                )])),
                ..ItemLookupInfo::default()
            },
            ..ferrofin_model::providers::SongInfo::default()
        }];
        assert_eq!(album_artist_mbid(&request).as_deref(), Some("song"));
        request.song_infos.clear();
        assert!(album_artist_mbid(&request).is_none());
    }

    #[tokio::test]
    async fn musicbrainz_artist_identify_searches_by_name_then_accent() {
        use super::MusicBrainzArtistSearchProvider;
        // The plain phrase search is empty; the `artistaccent:` retry hits.
        let (_server, mb) = musicbrainz_over(vec![
            (
                "query=artistaccent",
                r#"{"artists":[{"id":"bjork-mbid","name":"Björk","life-span":{"begin":"1965-11-21"}}]}"#.to_owned(),
            ),
            ("/ws/2/artist?", r#"{"artists":[]}"#.to_owned()),
        ])
        .await;
        let provider = MusicBrainzArtistSearchProvider::new(mb);
        assert_eq!(provider.name(), "MusicBrainz");
        assert!(provider.supports(BaseItemKind::MusicArtist));
        assert!(!provider.supports(BaseItemKind::MusicAlbum));

        let results = provider
            .get_search_results(&named_request(BaseItemKind::MusicArtist, "Björk"))
            .await
            .expect("results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name.as_deref(), Some("Björk"));
        assert_eq!(results[0].production_year, Some(1965));
        assert_eq!(
            results[0].premiere_date.map(|d| d.to_rfc3339()),
            Some("1965-11-21T00:00:00+00:00".to_owned())
        );
        assert_eq!(
            results[0].search_provider_name.as_deref(),
            Some("MusicBrainz")
        );
        assert_eq!(
            results[0].provider_ids.as_ref().expect("ids")["MusicBrainzArtist"],
            "bjork-mbid"
        );

        // An ASCII name with no hit does not retry: one empty result.
        let results = provider
            .get_search_results(&named_request(BaseItemKind::MusicArtist, "Nobody"))
            .await
            .expect("results");
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn musicbrainz_artist_identify_resolves_a_known_id_exactly() {
        use super::MusicBrainzArtistSearchProvider;
        let (_server, mb) = musicbrainz_over(vec![(
            "/ws/2/artist/artist-mbid",
            r#"{"id":"artist-mbid","name":"Miles Davis","life-span":{"begin":"1926"}}"#.to_owned(),
        )])
        .await;
        let provider = MusicBrainzArtistSearchProvider::new(mb);
        // The song's `MusicBrainzAlbumArtist` is the fallback for the artist's own.
        let mut request = named_request(BaseItemKind::MusicArtist, "ignored");
        request.song_infos = vec![ferrofin_model::providers::SongInfo {
            base: ItemLookupInfo {
                provider_ids: Some(HashMap::from([(
                    "MusicBrainzAlbumArtist".to_owned(),
                    "artist-mbid".to_owned(),
                )])),
                ..ItemLookupInfo::default()
            },
            ..ferrofin_model::providers::SongInfo::default()
        }];
        let results = provider
            .get_search_results(&request)
            .await
            .expect("results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name.as_deref(), Some("Miles Davis"));
        assert_eq!(results[0].production_year, Some(1926));
    }

    #[tokio::test]
    async fn audiodb_identify_is_empty_for_both_music_kinds() {
        use super::AudioDbSearchProvider;
        let album = AudioDbSearchProvider::new(BaseItemKind::MusicAlbum);
        let artist = AudioDbSearchProvider::new(BaseItemKind::MusicArtist);
        assert_eq!(album.name(), "TheAudioDB");
        assert!(album.supports(BaseItemKind::MusicAlbum));
        assert!(!album.supports(BaseItemKind::MusicArtist));
        assert!(artist.supports(BaseItemKind::MusicArtist));
        assert!(
            album
                .get_search_results(&named_request(BaseItemKind::MusicAlbum, "Kind of Blue"))
                .await
                .expect("ok")
                .is_empty()
        );
        assert!(
            artist
                .get_search_results(&named_request(BaseItemKind::MusicArtist, "Miles Davis"))
                .await
                .expect("ok")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn tmdb_person_identify_searches_by_name_or_resolves_a_tmdb_id() {
        use super::TmdbPersonSearchProvider;
        let server = crate::mock_http::MockServer::start(vec![
            (
                "/search/person",
                r#"{"results":[{"id":287,"name":"Brad Pitt","profile_path":"/bp.jpg"}]}"#.to_owned(),
            ),
            (
                "/person/287",
                r#"{"id":287,"name":"Brad Pitt","biography":"An actor.","images":{"profiles":[{"file_path":"/first.jpg"}]},"external_ids":{"imdb_id":"nm0000093"}}"#.to_owned(),
            ),
        ])
        .await;
        let tmdb = Arc::new(crate::tmdb::TmdbClient::new().with_base_url(&server.base_url));
        let provider = TmdbPersonSearchProvider::new(tmdb);
        assert_eq!(provider.name(), "TheMovieDb");
        assert!(provider.supports(BaseItemKind::Person));
        assert!(!provider.supports(BaseItemKind::Movie));

        // By name: name + profile image + Tmdb id, no overview.
        let results = provider
            .get_search_results(&named_request(BaseItemKind::Person, "Brad Pitt"))
            .await
            .expect("results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name.as_deref(), Some("Brad Pitt"));
        assert_eq!(
            results[0].image_url.as_deref(),
            Some("https://image.tmdb.org/t/p/original/bp.jpg")
        );
        assert_eq!(
            results[0].search_provider_name.as_deref(),
            Some("TheMovieDb")
        );
        let ids = results[0].provider_ids.as_ref().expect("ids");
        assert_eq!(ids["Tmdb"], "287");
        assert!(!ids.contains_key("Imdb"));
        assert!(results[0].overview.is_none());

        // By id: the lookup branch adds the biography + IMDb id.
        let results = provider
            .get_search_results(&id_request(BaseItemKind::Person, &[("tmdb", "287")]))
            .await
            .expect("results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].overview.as_deref(), Some("An actor."));
        assert_eq!(
            results[0].image_url.as_deref(),
            Some("https://image.tmdb.org/t/p/original/first.jpg")
        );
        let ids = results[0].provider_ids.as_ref().expect("ids");
        assert_eq!(ids["Tmdb"], "287");
        assert_eq!(ids["Imdb"], "nm0000093");

        // No name and no id → nothing to search.
        assert!(
            provider
                .get_search_results(&request(BaseItemKind::Person))
                .await
                .expect("ok")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn omdb_trailer_identify_supports_trailers_as_movies() {
        use super::OmdbSearchProvider;
        let server = crate::mock_http::MockServer::start(vec![(
            "type=movie",
            r#"{"Search":[{"Title":"Inception Trailer","Year":"2010","imdbID":"tt1375666","Type":"movie","Poster":"https://x/p.jpg"}],"Response":"True"}"#.to_owned(),
        )])
        .await;
        let omdb = Arc::new(crate::omdb::OmdbClient::new("key").with_base_url(&server.base_url));
        let provider = OmdbSearchProvider::for_trailers(omdb);
        assert_eq!(provider.name(), "The Open Movie Database");
        assert!(provider.supports(BaseItemKind::Trailer));
        assert!(!provider.supports(BaseItemKind::Movie));
        let results = provider
            .get_search_results(&named_request(BaseItemKind::Trailer, "Inception"))
            .await
            .expect("results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name.as_deref(), Some("Inception Trailer"));
        assert_eq!(results[0].production_year, Some(2010));
        assert_eq!(
            results[0].provider_ids.as_ref().expect("ids")["Imdb"],
            "tt1375666"
        );
    }

    #[tokio::test]
    async fn remote_search_is_empty_when_no_provider_registered() {
        let mgr = LocalProviderManager::default();
        let out = mgr
            .remote_search(&request(BaseItemKind::Movie))
            .await
            .expect("search succeeds");
        assert!(out.is_empty(), "no registered provider → []");
    }

    #[tokio::test]
    async fn remote_search_stamps_provider_name_and_returns_results() {
        let provider = Arc::new(FakeProvider {
            name: "TheMovieDb".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![result_with("Inception", &[("Tmdb", "27205")], None)],
            fail: false,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_search_providers(vec![provider as Arc<dyn RemoteSearchProvider>]);

        let out = mgr
            .remote_search(&request(BaseItemKind::Movie))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].search_provider_name.as_deref(), Some("TheMovieDb"));
        assert_eq!(out[0].name.as_deref(), Some("Inception"));
    }

    #[tokio::test]
    async fn remote_search_skips_providers_of_other_kinds() {
        let provider = Arc::new(FakeProvider {
            name: "MusicBrainz".to_owned(),
            kind: BaseItemKind::MusicAlbum,
            results: vec![result_with(
                "Kind of Blue",
                &[("MusicBrainzAlbum", "x")],
                None,
            )],
            fail: false,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_search_providers(vec![provider as Arc<dyn RemoteSearchProvider>]);
        // A Movie search must not reach a MusicAlbum provider.
        let out = mgr
            .remote_search(&request(BaseItemKind::Movie))
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn remote_search_provider_name_filter_is_case_insensitive() {
        let provider = Arc::new(FakeProvider {
            name: "TheMovieDb".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![result_with("Inception", &[("Tmdb", "27205")], None)],
            fail: false,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_search_providers(vec![provider as Arc<dyn RemoteSearchProvider>]);

        let mut req = request(BaseItemKind::Movie);
        req.search_provider_name = Some("themoviedb".to_owned());
        assert_eq!(mgr.remote_search(&req).await.unwrap().len(), 1);

        req.search_provider_name = Some("NoSuchProvider".to_owned());
        assert!(mgr.remote_search(&req).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn remote_search_merges_duplicates_by_shared_provider_id() {
        // Two providers return the same film under a shared Tmdb id; the second
        // adds an Imdb id and an image url the first lacked.
        let a = Arc::new(FakeProvider {
            name: "A".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![result_with("Inception", &[("Tmdb", "27205")], None)],
            fail: false,
        });
        let b = Arc::new(FakeProvider {
            name: "B".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![result_with(
                "Inception",
                &[("Tmdb", "27205"), ("Imdb", "tt1375666")],
                Some("http://img/incep.jpg"),
            )],
            fail: false,
        });
        let mgr = LocalProviderManager::default().with_remote_search_providers(vec![
            a as Arc<dyn RemoteSearchProvider>,
            b as Arc<dyn RemoteSearchProvider>,
        ]);

        let out = mgr
            .remote_search(&request(BaseItemKind::Movie))
            .await
            .unwrap();
        assert_eq!(out.len(), 1, "duplicates collapse to one entry");
        let ids = out[0].provider_ids.as_ref().unwrap();
        assert_eq!(ids["Tmdb"], "27205");
        assert_eq!(ids["Imdb"], "tt1375666", "missing id absorbed from B");
        assert_eq!(
            out[0].image_url.as_deref(),
            Some("http://img/incep.jpg"),
            "image url adopted from B"
        );
        // The first provider that produced the surviving entry wins its name.
        assert_eq!(out[0].search_provider_name.as_deref(), Some("A"));
    }

    #[tokio::test]
    async fn tvdb_search_provider_supports_series_and_guards_empty_name() {
        use super::TvdbSearchProvider;
        let p = TvdbSearchProvider::new(Arc::new(crate::tvdb::TvdbClient::new()));
        assert_eq!(p.name(), "TheTVDB");
        assert!(p.supports(BaseItemKind::Series));
        assert!(!p.supports(BaseItemKind::Movie));
        // An empty search name short-circuits before any network call.
        let out = p
            .get_search_results(&request(BaseItemKind::Series))
            .await
            .expect("empty");
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn remote_search_distinct_ids_are_not_merged() {
        let a = Arc::new(FakeProvider {
            name: "A".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![
                result_with("Inception", &[("Tmdb", "27205")], None),
                result_with("Tenet", &[("Tmdb", "577922")], None),
            ],
            fail: false,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_search_providers(vec![a as Arc<dyn RemoteSearchProvider>]);
        let out = mgr
            .remote_search(&request(BaseItemKind::Movie))
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn remote_search_swallows_a_failing_provider() {
        let bad = Arc::new(FakeProvider {
            name: "Bad".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![],
            fail: true,
        });
        let good = Arc::new(FakeProvider {
            name: "Good".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![result_with("Dune", &[("Tmdb", "438631")], None)],
            fail: false,
        });
        let mgr = LocalProviderManager::default().with_remote_search_providers(vec![
            bad as Arc<dyn RemoteSearchProvider>,
            good as Arc<dyn RemoteSearchProvider>,
        ]);
        let out = mgr
            .remote_search(&request(BaseItemKind::Movie))
            .await
            .unwrap();
        assert_eq!(out.len(), 1, "the good provider's result still lands");
        assert_eq!(out[0].name.as_deref(), Some("Dune"));
    }

    #[tokio::test]
    async fn external_id_infos_are_advertised() {
        let mgr = LocalProviderManager::default();
        let infos = mgr
            .get_external_id_infos(Uuid::nil())
            .await
            .expect("descriptor query succeeds");
        assert!(infos.is_empty());
    }

    /// An [`ItemTypeLookup`] over the two names the descriptor tests need.
    struct FakeTypes;

    impl ferrofin_traits::persistence::ItemTypeLookup for FakeTypes {
        fn music_genre_types(&self) -> Vec<String> {
            Vec::new()
        }
        fn base_item_kind_names(&self) -> HashMap<BaseItemKind, String> {
            HashMap::from([
                (
                    BaseItemKind::Movie,
                    "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
                ),
                (
                    BaseItemKind::Book,
                    "MediaBrowser.Controller.Entities.Book".to_owned(),
                ),
            ])
        }
    }

    /// Builds a manager over one row of `type_name`, wired for kind filtering.
    fn manager_over(item_id: Uuid, type_name: &str) -> LocalProviderManager {
        let mut item = row("Book", "irrelevant");
        item.id = item_id.to_string();
        item.type_ = type_name.to_owned();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let items = Arc::new(FakeItems {
            rows: HashMap::from([(item_id, item)]),
            seen: tx,
        });
        LocalProviderManager::default()
            .with_remote_images(Arc::new(crate::tmdb::TmdbClient::new()), items)
            .with_item_types(&FakeTypes)
    }

    #[tokio::test]
    async fn external_id_infos_are_filtered_to_the_items_kind() {
        let item_id = Uuid::new_v4();
        let mgr = manager_over(item_id, "MediaBrowser.Controller.Entities.Movies.Movie");
        let keys: Vec<_> = mgr
            .get_external_id_infos(item_id)
            .await
            .expect("descriptors")
            .into_iter()
            .filter_map(|i| i.key)
            .collect();
        assert!(keys.contains(&"Imdb".to_owned()));
        assert!(keys.contains(&"Tmdb".to_owned()));
        assert!(
            !keys.contains(&"ISBN".to_owned()),
            "a movie must not offer a book id field"
        );
    }

    #[tokio::test]
    async fn a_book_offers_only_the_book_id_fields() {
        let item_id = Uuid::new_v4();
        let mgr = manager_over(item_id, "MediaBrowser.Controller.Entities.Book");
        let keys: Vec<_> = mgr
            .get_external_id_infos(item_id)
            .await
            .expect("descriptors")
            .into_iter()
            .filter_map(|i| i.key)
            .collect();
        assert_eq!(keys, vec!["ComicVine", "GoogleBooks", "ISBN"]);
    }

    #[tokio::test]
    async fn an_unknown_item_falls_back_to_the_supplied_descriptors() {
        let seed = vec![ExternalIdInfo::new("Tmdb".into(), "tmdb".into(), None)];
        let mgr = LocalProviderManager::new(seed.clone())
            .with_item_types(&FakeTypes)
            .with_remote_search_providers(Vec::new());
        // No item store wired, so the kind is unknowable: only the seed shows.
        assert_eq!(
            mgr.get_external_id_infos(Uuid::new_v4()).await.unwrap(),
            seed
        );
    }

    #[tokio::test]
    async fn new_advertises_supplied_external_ids_for_every_item() {
        let seed = vec![ExternalIdInfo::new("Tmdb".into(), "tmdb".into(), None)];
        let mgr = LocalProviderManager::new(seed.clone());
        // The manager clones the same descriptor set for any item id.
        let a = mgr.get_external_id_infos(Uuid::nil()).await.unwrap();
        let b = mgr.get_external_id_infos(Uuid::new_v4()).await.unwrap();
        assert_eq!(a, seed);
        assert_eq!(b, seed);
    }

    #[tokio::test]
    async fn refresh_single_item_without_a_provider_changes_nothing() {
        let mgr = LocalProviderManager::default();
        let update = mgr
            .refresh_single_item(Uuid::nil(), &MetadataRefreshOptions::default())
            .await
            .expect("no provider → nothing to do");
        assert_eq!(update, ItemUpdateType::None);
    }

    /// Every image write on a manager built without the image store must
    /// return a backend error naming the op.
    #[tokio::test]
    async fn image_writes_without_a_store_name_the_op() {
        let mgr = LocalProviderManager::default();
        let id = Uuid::nil();
        let opts = MetadataRefreshOptions::default();

        // `queue_refresh` spawns the refresh in the background and returns Ok
        // immediately; with no TMDB client wired the spawned refresh no-ops.
        mgr.queue_refresh(id, &opts, RefreshPriority::Normal)
            .await
            .expect("queue_refresh accepts the enqueue");

        // `refresh_full_item` on a manager with no TMDB client / item store has
        // nothing to fetch and succeeds as a no-op (faithful: Jellyfin with no
        // metadata provider leaves the item unchanged). The field-merge helpers it
        // applies are unit-tested in `metadata_merge_helpers` below.
        mgr.refresh_full_item(id, &opts)
            .await
            .expect("refresh_full_item is a no-op without a provider");

        let save_url = mgr
            .save_image_from_url(id, "http://x/y.jpg", ImageType::Primary, Some(0))
            .await
            .expect_err("save_image_from_url unwired");
        assert!(save_url.to_string().contains("save_image_from_url"));

        let save_bytes = mgr
            .save_image(id, b"data", "image/jpeg", ImageType::Backdrop, None)
            .await
            .expect_err("save_image unwired");
        assert!(save_bytes.to_string().contains("save_image"));

        // `save_metadata` is an accepted no-op (the DB write happens in the
        // caller), so the image-download / metadata-edit flows complete rather
        // than 500.
        mgr.save_metadata(id, ItemUpdateType::MetadataDownload)
            .await
            .expect("save_metadata is an accepted no-op");
    }

    /// The field-merge helpers behind `refresh_full_item`: `wants_fetch` gates on
    /// the refresh mode, `set_text` fills-or-replaces, `parse_ymd` reads a TMDB
    /// date.
    #[test]
    fn metadata_merge_helpers() {
        // wants_fetch: Default/FullRefresh fetch; None/ValidationOnly do not.
        assert!(wants_fetch(MetadataRefreshMode::FullRefresh));
        assert!(wants_fetch(MetadataRefreshMode::Default));
        assert!(!wants_fetch(MetadataRefreshMode::None));
        assert!(!wants_fetch(MetadataRefreshMode::ValidationOnly));

        // set_text fills an empty target regardless of replace.
        let mut cur = None;
        set_text(&mut cur, Some("Solaris"), false);
        assert_eq!(cur.as_deref(), Some("Solaris"));
        // With replace=false an existing value is kept.
        set_text(&mut cur, Some("Stalker"), false);
        assert_eq!(cur.as_deref(), Some("Solaris"));
        // With replace=true it is overwritten.
        set_text(&mut cur, Some("Stalker"), true);
        assert_eq!(cur.as_deref(), Some("Stalker"));
        // A `None` incoming value never clears the target.
        set_text(&mut cur, None, true);
        assert_eq!(cur.as_deref(), Some("Stalker"));
        // An empty existing string counts as absent (fills without replace).
        let mut empty = Some(String::new());
        set_text(&mut empty, Some("Mirror"), false);
        assert_eq!(empty.as_deref(), Some("Mirror"));

        // parse_ymd reads a valid date and rejects garbage.
        assert!(parse_ymd("1972-03-20").is_some());
        assert!(parse_ymd("not-a-date").is_none());
    }

    /// `apply_tmdb_details` fills empty fields, honors `replace`, joins
    /// genres/studios, converts runtime to ticks, and parses the premiere date.
    #[test]
    fn apply_tmdb_details_fills_and_replaces() {
        let details = TmdbDetails {
            name: Some("Solaris".to_owned()),
            original_title: Some("Солярис".to_owned()),
            overview: Some("A physicist visits a space station.".to_owned()),
            tagline: Some("A tagline".to_owned()),
            genres: vec!["Science Fiction".to_owned(), "Drama".to_owned()],
            studios: vec!["Mosfilm".to_owned()],
            community_rating: Some(8.1),
            official_rating: Some("PG".to_owned()),
            production_year: Some(1972),
            premiere_date: Some("1972-03-20".to_owned()),
            runtime_minutes: Some(167),
            ..TmdbDetails::default()
        };

        // Empty entity ⇒ every field filled regardless of `replace`.
        let mut entity = BaseItemEntity::default();
        apply_tmdb_details(&mut entity, &details, false);
        assert_eq!(
            entity.overview.as_deref(),
            Some("A physicist visits a space station.")
        );
        assert_eq!(entity.genres.as_deref(), Some("Science Fiction|Drama"));
        assert_eq!(entity.studios.as_deref(), Some("Mosfilm"));
        assert_eq!(entity.community_rating, Some(8.1));
        assert_eq!(entity.official_rating.as_deref(), Some("PG"));
        assert_eq!(entity.production_year, Some(1972));
        assert_eq!(entity.run_time_ticks, Some(167 * 600_000_000));
        assert!(entity.premiere_date.is_some());

        // With replace=false an existing value is kept.
        let mut kept = BaseItemEntity {
            overview: Some("existing".to_owned()),
            ..BaseItemEntity::default()
        };
        apply_tmdb_details(&mut kept, &details, false);
        assert_eq!(kept.overview.as_deref(), Some("existing"));
        // With replace=true it is overwritten.
        apply_tmdb_details(&mut kept, &details, true);
        assert_eq!(
            kept.overview.as_deref(),
            Some("A physicist visits a space station.")
        );

        // A detail TMDB did not return leaves the field untouched.
        let mut untouched = BaseItemEntity {
            tagline: Some("keep me".to_owned()),
            ..BaseItemEntity::default()
        };
        apply_tmdb_details(&mut untouched, &TmdbDetails::default(), true);
        assert_eq!(untouched.tagline.as_deref(), Some("keep me"));

        // `MergeBaseItemData`: the name (and its sort key) follow the fetched
        // title only on replace; a `ForcedSortName` pins the sort key.
        let mut named = BaseItemEntity {
            name: Some("Alpha".to_owned()),
            sort_name: Some("alpha".to_owned()),
            ..BaseItemEntity::default()
        };
        apply_tmdb_details(&mut named, &details, false);
        assert_eq!(named.name.as_deref(), Some("Alpha"));
        assert_eq!(named.sort_name.as_deref(), Some("alpha"));
        apply_tmdb_details(&mut named, &details, true);
        assert_eq!(named.name.as_deref(), Some("Solaris"));
        assert_eq!(named.original_title.as_deref(), Some("Солярис"));
        assert_eq!(named.sort_name.as_deref(), Some("solaris"));
        let mut pinned = BaseItemEntity {
            name: Some("Alpha".to_owned()),
            sort_name: Some("zzz".to_owned()),
            forced_sort_name: Some("zzz".to_owned()),
            ..BaseItemEntity::default()
        };
        apply_tmdb_details(&mut pinned, &details, true);
        assert_eq!(pinned.name.as_deref(), Some("Solaris"));
        assert_eq!(pinned.sort_name.as_deref(), Some("zzz"));
    }

    /// The read-only descriptor queries return empty/default results (no store,
    /// no remote providers wired in First-Light) — never an error.
    #[tokio::test]
    async fn read_only_queries_return_empty_defaults() {
        let mgr = LocalProviderManager::default();
        let id = Uuid::new_v4();
        let query = RemoteImageQuery::new("Tmdb".into());

        assert!(
            mgr.get_available_remote_images(id, &query)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            mgr.get_remote_image_provider_info(id)
                .await
                .unwrap()
                .is_empty()
        );
        // The metadata-plugin registry projects the compiled-in providers, so it
        // is non-empty (covered in detail by `library_options` tests).
        assert!(!mgr.get_all_metadata_plugins().await.unwrap().is_empty());
        assert!(mgr.get_refresh_queue().await.unwrap().is_empty());

        // Metadata options fall back to the type default.
        let opts = mgr.get_metadata_options(id).await.unwrap();
        assert_eq!(
            opts,
            ferrofin_model::configuration::MetadataOptions::default()
        );
    }

    /// An [`ItemRepository`] over a fixed map, reporting each `retrieve_item`
    /// call on a channel so a test can observe a background refresh running.
    /// Every method the refresh path never touches is `unimplemented!`.
    struct FakeItems {
        rows: HashMap<Uuid, BaseItemEntity>,
        seen: tokio::sync::mpsc::UnboundedSender<Uuid>,
    }

    #[async_trait]
    impl ferrofin_traits::persistence::ItemRepository for FakeItems {
        async fn retrieve_item(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
            let _ = self.seen.send(id);
            Ok(self.rows.get(&id).cloned())
        }
        async fn locked_item_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
            Ok(self
                .rows
                .iter()
                .filter(|(_, row)| row.is_locked)
                .map(|(id, _)| *id)
                .collect())
        }
        async fn item_text_rows(
            &self,
            _kind: ferrofin_model::data::BaseItemKind,
            _ids: &[Uuid],
        ) -> Result<Vec<ferrofin_db::entities::base_items::ItemTextRow>, ServiceError> {
            unimplemented!()
        }
        async fn get_ancestor_chain(
            &self,
            _item_id: Uuid,
        ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError> {
            unimplemented!()
        }
        async fn get_items(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<ferrofin_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
            unimplemented!()
        }
        async fn get_item_ids(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<Vec<Uuid>, ServiceError> {
            unimplemented!()
        }
        async fn get_item_list(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            unimplemented!()
        }
        async fn get_latest_item_list(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
            _collection_type: ferrofin_model::data::CollectionType,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            unimplemented!()
        }
        async fn item_exists(&self, _id: Uuid) -> Result<bool, ServiceError> {
            unimplemented!()
        }
        async fn get_items_by_primary_version(
            &self,
            _primary_id: Uuid,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            unimplemented!()
        }
        async fn get_items_with_provider_id(
            &self,
            _provider_key: &str,
        ) -> Result<Vec<(Uuid, String)>, ServiceError> {
            unimplemented!()
        }
        async fn get_image_infos(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_traits::options::ItemImageInfo>, ServiceError> {
            unimplemented!()
        }
        async fn swap_item_images(
            &self,
            _item_id: Uuid,
            _image_type: ImageType,
            _index1: i32,
            _index2: i32,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn get_genres(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_music_genres(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_studios(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_artists(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_album_artists(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_all_artists(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_music_genre_names(&self) -> Result<Vec<String>, ServiceError> {
            unimplemented!()
        }
        async fn get_studio_names(&self) -> Result<Vec<String>, ServiceError> {
            unimplemented!()
        }
        async fn get_genre_names(&self) -> Result<Vec<String>, ServiceError> {
            unimplemented!()
        }
        async fn get_all_artist_names(&self) -> Result<Vec<String>, ServiceError> {
            unimplemented!()
        }
        async fn get_media_stream_languages(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
            _stream_type: ferrofin_model::entities::MediaStreamType,
        ) -> Result<Vec<String>, ServiceError> {
            unimplemented!()
        }
        async fn get_query_filters_legacy(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
            unimplemented!()
        }
        async fn get_is_played(
            &self,
            _user: &ferrofin_db::entities::users::UserEntity,
            _id: Uuid,
            _recursive: bool,
        ) -> Result<bool, ServiceError> {
            unimplemented!()
        }
        async fn get_playlist_items_with_access(
            &self,
            _playlist_id: Uuid,
            _user_id: Uuid,
            _child_type: i32,
        ) -> Result<ferrofin_traits::persistence::PlaylistItemsWithAccess, ServiceError> {
            unimplemented!()
        }
    }

    /// A minimal row of the given stored C# type name.
    fn row(kind: &str, name: &str) -> BaseItemEntity {
        BaseItemEntity {
            id: Uuid::new_v4().to_string(),
            name: Some(name.to_owned()),
            type_: format!("MediaBrowser.Controller.Entities.{kind}"),
            ..BaseItemEntity::default()
        }
    }

    #[tokio::test]
    async fn studio_remote_images_return_the_repository_thumb() {
        // A Studio item resolves its Thumb from the artwork repository, matched by
        // normalized name; the studios client's manifest is seeded so no network
        // is touched.
        let item_id = Uuid::new_v4();
        let mut studio = row("Studio", "Walt Disney Pictures");
        studio.id = item_id.to_string();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let items = Arc::new(FakeItems {
            rows: HashMap::from([(item_id, studio)]),
            seen: tx,
        });
        let studios = Arc::new(crate::studios::StudiosClient::new());
        studios.seed_manifest(vec!["Walt Disney Pictures".to_owned()]);
        let mgr = LocalProviderManager::default()
            .with_remote_images(Arc::new(crate::tmdb::TmdbClient::new()), items)
            .with_studios(studios);

        let images = mgr
            .get_available_remote_images(item_id, &RemoteImageQuery::default())
            .await
            .expect("images");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].type_, ImageType::Thumb);
        assert_eq!(
            images[0].provider_name.as_deref(),
            Some("Artwork Repository")
        );
        assert!(
            images[0]
                .url
                .as_deref()
                .is_some_and(|u| u.ends_with("/images/Walt Disney Pictures/thumb.jpg"))
        );

        // The provider-info advertiser reports the Thumb-only studios provider.
        let info = mgr
            .get_remote_image_provider_info(item_id)
            .await
            .expect("info");
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].supported_images, vec![ImageType::Thumb]);
    }

    #[tokio::test]
    async fn studio_without_a_manifest_match_yields_no_image() {
        let item_id = Uuid::new_v4();
        let mut studio = row("Studio", "An Unlisted Studio");
        studio.id = item_id.to_string();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let items = Arc::new(FakeItems {
            rows: HashMap::from([(item_id, studio)]),
            seen: tx,
        });
        let studios = Arc::new(crate::studios::StudiosClient::new());
        studios.seed_manifest(vec!["Netflix".to_owned()]);
        let mgr = LocalProviderManager::default()
            .with_remote_images(Arc::new(crate::tmdb::TmdbClient::new()), items)
            .with_studios(studios);

        let images = mgr
            .get_available_remote_images(item_id, &RemoteImageQuery::default())
            .await
            .expect("images");
        assert!(images.is_empty());
    }

    /// A stored `(provider key, value)` id list.
    type IdPairs = Vec<(String, String)>;

    /// An [`ItemPersistenceService`] fake recording the writes a refresh makes
    /// (replaced id sets, upserted ids, saved rows) over a seeded id store.
    #[derive(Default)]
    struct RecordingStore {
        stored_ids: std::sync::Mutex<HashMap<Uuid, Vec<(String, String)>>>,
        replaced: std::sync::Mutex<Vec<(Uuid, IdPairs)>>,
        upserted: std::sync::Mutex<Vec<(Uuid, String, String)>>,
        saved: std::sync::Mutex<Vec<BaseItemEntity>>,
    }

    #[async_trait]
    impl ferrofin_traits::persistence::ItemPersistenceService for RecordingStore {
        async fn delete_items(&self, _ids: &[Uuid]) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn save_items(&self, items: &[BaseItemEntity]) -> Result<(), ServiceError> {
            self.saved.lock().expect("lock").extend_from_slice(items);
            Ok(())
        }
        async fn set_primary_version_id(
            &self,
            _item_id: Uuid,
            _primary_version_id: Option<Uuid>,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn save_item_values(
            &self,
            _item_id: Uuid,
            _values: &[(i32, String)],
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn item_exists(&self, _id: Uuid) -> Result<bool, ServiceError> {
            unimplemented!()
        }
        async fn set_parent_id(
            &self,
            _item_id: Uuid,
            _parent_id: Uuid,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn set_collection_type(
            &self,
            _item_id: Uuid,
            _collection_type: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn set_ancestors(
            &self,
            _item_id: Uuid,
            _ancestor_ids: &[Uuid],
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn save_images(&self, _item: &BaseItemEntity) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn provider_ids_for_items(
            &self,
            item_ids: &[Uuid],
        ) -> Result<HashMap<Uuid, Vec<(String, String)>>, ServiceError> {
            let stored = self.stored_ids.lock().expect("lock");
            Ok(item_ids
                .iter()
                .filter_map(|id| stored.get(id).map(|ids| (*id, ids.clone())))
                .collect())
        }
        async fn save_provider_id(
            &self,
            item_id: Uuid,
            provider: &str,
            value: &str,
        ) -> Result<(), ServiceError> {
            self.upserted.lock().expect("lock").push((
                item_id,
                provider.to_owned(),
                value.to_owned(),
            ));
            Ok(())
        }
        async fn replace_provider_ids(
            &self,
            item_id: Uuid,
            ids: &[(String, String)],
        ) -> Result<(), ServiceError> {
            self.replaced
                .lock()
                .expect("lock")
                .push((item_id, ids.to_vec()));
            self.stored_ids
                .lock()
                .expect("lock")
                .insert(item_id, ids.to_vec());
            Ok(())
        }
        async fn reattach_user_data(&self, _item: &BaseItemEntity) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn update_inherited_values(&self) -> Result<(), ServiceError> {
            unimplemented!()
        }
    }

    /// A mock TMDB where the title search points at movie 999 ("Wrong Pick")
    /// while id 603 ("The Matrix") is only reachable directly or via `/find`.
    async fn tmdb_with_decoy_search() -> (crate::mock_http::MockServer, Arc<crate::tmdb::TmdbClient>)
    {
        let server = crate::mock_http::MockServer::start(vec![
            (
                "/search/movie",
                r#"{"results":[{"id":999,"title":"Wrong Pick","release_date":"1999-03-31"}]}"#
                    .to_owned(),
            ),
            (
                "/find/tt0133093",
                r#"{"movie_results":[{"id":603}],"tv_results":[]}"#.to_owned(),
            ),
            (
                "/movie/603",
                r#"{"id":603,"title":"The Matrix","overview":"Neo.","imdb_id":"tt0133093"}"#
                    .to_owned(),
            ),
            (
                "/movie/999",
                r#"{"id":999,"title":"Wrong Pick","overview":"Decoy.","imdb_id":"tt0000999"}"#
                    .to_owned(),
            ),
        ])
        .await;
        let tmdb = Arc::new(crate::tmdb::TmdbClient::new().with_base_url(&server.base_url));
        (server, tmdb)
    }

    /// A movie row `name` under a manager wired with `tmdb` + `store`.
    fn movie_manager(
        name: &str,
        tmdb: Arc<crate::tmdb::TmdbClient>,
        store: Arc<RecordingStore>,
    ) -> (Uuid, LocalProviderManager) {
        let item_id = Uuid::new_v4();
        let mut movie = row("Movies.Movie", name);
        movie.id = item_id.to_string();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let items = Arc::new(FakeItems {
            rows: HashMap::from([(item_id, movie)]),
            seen: tx,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_images(tmdb, items)
            .with_image_store(store, std::env::temp_dir());
        (item_id, mgr)
    }

    fn full_metadata_refresh() -> MetadataRefreshOptions {
        MetadataRefreshOptions {
            metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
            image_refresh_mode: MetadataRefreshMode::None,
            replace_all_metadata: true,
            replace_all_images: false,
            search_result: None,
        }
    }

    #[tokio::test]
    async fn apply_search_result_binds_the_refresh_to_the_chosen_id() {
        // "Identify → Apply": the chosen result's ids replace the item's and the
        // fetch goes straight to that TMDB id — the decoy title search is never
        // what gets applied.
        let (_server, tmdb) = tmdb_with_decoy_search().await;
        let store = Arc::new(RecordingStore::default());
        let (item_id, mgr) = movie_manager("Matrix", tmdb, Arc::clone(&store));

        let chosen = RemoteSearchResult {
            name: Some("The Matrix".to_owned()),
            production_year: Some(1999),
            provider_ids: Some(HashMap::from([("Tmdb".to_owned(), "603".to_owned())])),
            ..RemoteSearchResult::default()
        };
        mgr.refresh_full_item(
            item_id,
            &MetadataRefreshOptions {
                search_result: Some(chosen),
                ..full_metadata_refresh()
            },
        )
        .await
        .expect("refresh");

        // The id set was replaced wholesale with the result's.
        assert_eq!(
            *store.replaced.lock().expect("lock"),
            vec![(item_id, vec![("Tmdb".to_owned(), "603".to_owned())])]
        );
        // The row carries 603's metadata, not the decoy's.
        let saved = store.saved.lock().expect("lock");
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].overview.as_deref(), Some("Neo."));
        // The fetch's own ids (TMDB + IMDb) were upserted alongside.
        let upserted = store.upserted.lock().expect("lock");
        assert!(upserted.contains(&(item_id, "Tmdb".to_owned(), "603".to_owned())));
        assert!(upserted.contains(&(item_id, "Imdb".to_owned(), "tt0133093".to_owned())));
    }

    #[tokio::test]
    async fn refresh_prefers_the_items_stored_external_id_over_a_title_search() {
        // A plain refresh of an item that already carries an IMDb id resolves
        // it through `/find` instead of re-searching by title.
        let (_server, tmdb) = tmdb_with_decoy_search().await;
        let store = Arc::new(RecordingStore::default());
        let (item_id, mgr) = movie_manager("Matrix", tmdb, Arc::clone(&store));
        store
            .stored_ids
            .lock()
            .expect("lock")
            .insert(item_id, vec![("imdb".to_owned(), "tt0133093".to_owned())]);

        mgr.refresh_full_item(item_id, &full_metadata_refresh())
            .await
            .expect("refresh");

        assert!(store.replaced.lock().expect("lock").is_empty());
        let saved = store.saved.lock().expect("lock");
        assert_eq!(saved[0].overview.as_deref(), Some("Neo."));
    }

    #[tokio::test]
    async fn refresh_without_any_id_falls_back_to_the_title_search() {
        let (_server, tmdb) = tmdb_with_decoy_search().await;
        let store = Arc::new(RecordingStore::default());
        let (item_id, mgr) = movie_manager("Matrix", tmdb, Arc::clone(&store));

        mgr.refresh_full_item(item_id, &full_metadata_refresh())
            .await
            .expect("refresh");

        let saved = store.saved.lock().expect("lock");
        assert_eq!(saved[0].overview.as_deref(), Some("Decoy."));
    }

    /// A manager over one `kind` row named `name` with `ids` stored, wired with
    /// `tmdb` plus whatever other clients `wire` attaches.
    fn image_manager(
        kind: &str,
        name: &str,
        ids: &[(&str, &str)],
        tmdb: Arc<crate::tmdb::TmdbClient>,
        wire: impl FnOnce(LocalProviderManager) -> LocalProviderManager,
    ) -> (Uuid, LocalProviderManager) {
        let item_id = Uuid::new_v4();
        let mut entity = row(kind, name);
        entity.id = item_id.to_string();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let items = Arc::new(FakeItems {
            rows: HashMap::from([(item_id, entity)]),
            seen: tx,
        });
        let store = Arc::new(RecordingStore::default());
        store.stored_ids.lock().expect("lock").insert(
            item_id,
            ids.iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        );
        let mgr = LocalProviderManager::default()
            .with_remote_images(tmdb, items)
            .with_image_store(store, std::env::temp_dir());
        (item_id, wire(mgr))
    }

    const FANART_MUSIC: &str = r#"{"artistthumb":[{"url":"https://f/thumb.jpg","likes":"5"}],"musicbanner":[{"url":"https://f/banner.jpg"}],"albums":[{"release_group_id":"rg-1","albumcover":[{"url":"https://f/cover.jpg"}],"cdart":[{"url":"https://f/cd.png"}]}]}"#;
    const AUDIODB_ARTIST: &str = r#"{"artists":[{"strArtistThumb":"https://a/thumb.jpg","strArtistLogo":"https://a/logo.png","strArtistFanart":"https://a/fan.jpg"}]}"#;
    const AUDIODB_ALBUM: &str =
        r#"{"album":[{"strAlbumThumb":"https://a/cover.jpg","strAlbumCDart":"https://a/cd.png"}]}"#;

    #[tokio::test]
    async fn music_artist_remote_images_come_from_audiodb_and_fanart() {
        let fanart = crate::mock_http::MockServer::always(FANART_MUSIC).await;
        let audiodb = crate::mock_http::MockServer::always(AUDIODB_ARTIST).await;
        let (item_id, mgr) = image_manager(
            "Audio.MusicArtist",
            "Miles Davis",
            &[("MusicBrainzArtist", "artist-mbid")],
            Arc::new(crate::tmdb::TmdbClient::new()),
            |mgr| {
                mgr.with_fanart(Arc::new(
                    crate::fanart::FanartClient::new(None).with_base_url(&fanart.base_url),
                ))
                .with_audiodb(Arc::new(
                    crate::audiodb::AudioDbClient::with_base_url(&audiodb.base_url),
                ))
            },
        );

        // Provider info: both providers, each with its C# `GetSupportedImages`.
        let info = mgr.get_remote_image_provider_info(item_id).await.unwrap();
        let names: Vec<&str> = info.iter().filter_map(|i| i.name.as_deref()).collect();
        assert_eq!(names, ["TheAudioDB", "FanArt"]);
        assert_eq!(
            info[0].supported_images,
            [
                ImageType::Primary,
                ImageType::Logo,
                ImageType::Banner,
                ImageType::Backdrop
            ]
        );
        assert_eq!(
            info[1].supported_images,
            [
                ImageType::Primary,
                ImageType::Logo,
                ImageType::Art,
                ImageType::Banner,
                ImageType::Backdrop
            ]
        );

        // All images, in provider order, each stamped with its provider.
        let all = mgr
            .get_available_remote_images(item_id, &RemoteImageQuery::default())
            .await
            .unwrap();
        let by_provider: Vec<(&str, ImageType)> = all
            .iter()
            .map(|i| (i.provider_name.as_deref().unwrap_or_default(), i.type_))
            .collect();
        assert_eq!(
            by_provider,
            [
                ("TheAudioDB", ImageType::Primary),
                ("TheAudioDB", ImageType::Logo),
                ("TheAudioDB", ImageType::Backdrop),
                ("FanArt", ImageType::Primary),
                ("FanArt", ImageType::Banner),
            ]
        );

        // `ProviderName` narrows to one provider (case-insensitively) …
        let only_fanart = mgr
            .get_available_remote_images(item_id, &RemoteImageQuery::new("fanart".to_owned()))
            .await
            .unwrap();
        assert_eq!(only_fanart.len(), 2);
        assert!(
            only_fanart
                .iter()
                .all(|i| i.provider_name.as_deref() == Some("FanArt"))
        );
        // … and `ImageType` narrows within it.
        let banners = mgr
            .get_available_remote_images(
                item_id,
                &RemoteImageQuery {
                    provider_name: "FanArt".to_owned(),
                    image_type: Some(ImageType::Banner),
                    ..RemoteImageQuery::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(banners.len(), 1);
        assert_eq!(banners[0].url.as_deref(), Some("https://f/banner.jpg"));
        // An unknown provider name yields nothing.
        assert!(
            mgr.get_available_remote_images(item_id, &RemoteImageQuery::new("Nope".to_owned()))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn music_album_remote_images_need_the_release_group_id() {
        let fanart = crate::mock_http::MockServer::always(FANART_MUSIC).await;
        let audiodb = crate::mock_http::MockServer::always(AUDIODB_ALBUM).await;
        let wire = |mgr: LocalProviderManager| {
            mgr.with_fanart(Arc::new(
                crate::fanart::FanartClient::new(None).with_base_url(&fanart.base_url),
            ))
            .with_audiodb(Arc::new(crate::audiodb::AudioDbClient::with_base_url(
                &audiodb.base_url,
            )))
        };
        let (item_id, mgr) = image_manager(
            "Audio.MusicAlbum",
            "Kind of Blue",
            &[
                ("MusicBrainzReleaseGroup", "rg-1"),
                ("MusicBrainzAlbumArtist", "artist-mbid"),
            ],
            Arc::new(crate::tmdb::TmdbClient::new()),
            wire,
        );
        let info = mgr.get_remote_image_provider_info(item_id).await.unwrap();
        assert_eq!(info.len(), 2);
        assert_eq!(
            info[0].supported_images,
            [ImageType::Primary, ImageType::Disc]
        );
        assert_eq!(
            info[1].supported_images,
            [ImageType::Primary, ImageType::Disc]
        );

        let all = mgr
            .get_available_remote_images(item_id, &RemoteImageQuery::default())
            .await
            .unwrap();
        let urls: Vec<&str> = all.iter().filter_map(|i| i.url.as_deref()).collect();
        assert_eq!(
            urls,
            [
                "https://a/cover.jpg",
                "https://a/cd.png",
                "https://f/cover.jpg",
                "https://f/cd.png"
            ]
        );

        // Without the MusicBrainz ids neither provider can look anything up.
        let (bare_id, bare) = image_manager(
            "Audio.MusicAlbum",
            "Kind of Blue",
            &[],
            Arc::new(crate::tmdb::TmdbClient::new()),
            wire,
        );
        assert_eq!(
            bare.get_remote_image_provider_info(bare_id)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            bare.get_available_remote_images(bare_id, &RemoteImageQuery::default())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn movie_remote_images_list_tmdb_fanart_and_omdb_with_their_supported_types() {
        let server = crate::mock_http::MockServer::start(vec![
            (
                "/movie/603/images",
                r#"{"posters":[{"file_path":"/p.jpg","width":1000,"height":1500,"iso_639_1":"en"}],"backdrops":[{"file_path":"/b.jpg"},{"file_path":"/t.jpg","iso_639_1":"en"}],"logos":[{"file_path":"/l.png"}]}"#.to_owned(),
            ),
            (
                "/movies/603",
                r#"{"movieposter":[{"url":"https://f/poster.jpg","lang":"en","likes":"3"}],"hdmovielogo":[{"url":"https://f/logo.png"}]}"#.to_owned(),
            ),
            (
                "i=tt0133093",
                r#"{"Title":"The Matrix","Poster":"https://o/poster.jpg","Response":"True"}"#.to_owned(),
            ),
        ])
        .await;
        let base = server.base_url.clone();
        let (item_id, mgr) = image_manager(
            "Movies.Movie",
            "The Matrix",
            &[("Tmdb", "603"), ("Imdb", "tt0133093")],
            Arc::new(crate::tmdb::TmdbClient::new().with_base_url(&base)),
            |mgr| {
                mgr.with_fanart(Arc::new(
                    crate::fanart::FanartClient::new(None).with_base_url(&base),
                ))
                .with_omdb(Arc::new(
                    crate::omdb::OmdbClient::new("key").with_base_url(&base),
                ))
            },
        );

        let info = mgr.get_remote_image_provider_info(item_id).await.unwrap();
        let names: Vec<&str> = info.iter().filter_map(|i| i.name.as_deref()).collect();
        assert_eq!(names, ["TheMovieDb", "FanArt", "The Open Movie Database"]);
        assert_eq!(
            info[0].supported_images,
            [
                ImageType::Primary,
                ImageType::Backdrop,
                ImageType::Logo,
                ImageType::Thumb
            ]
        );
        assert_eq!(info[2].supported_images, [ImageType::Primary]);

        let all = mgr
            .get_available_remote_images(item_id, &RemoteImageQuery::default())
            .await
            .unwrap();
        let shape: Vec<(&str, ImageType, &str)> = all
            .iter()
            .map(|i| {
                (
                    i.provider_name.as_deref().unwrap_or_default(),
                    i.type_,
                    i.url.as_deref().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (
                    "TheMovieDb",
                    ImageType::Primary,
                    "https://image.tmdb.org/t/p/original/p.jpg"
                ),
                (
                    "TheMovieDb",
                    ImageType::Backdrop,
                    "https://image.tmdb.org/t/p/original/b.jpg"
                ),
                // A languaged backdrop is a Thumb (it carries text).
                (
                    "TheMovieDb",
                    ImageType::Thumb,
                    "https://image.tmdb.org/t/p/original/t.jpg"
                ),
                (
                    "TheMovieDb",
                    ImageType::Logo,
                    "https://image.tmdb.org/t/p/original/l.png"
                ),
                ("FanArt", ImageType::Primary, "https://f/poster.jpg"),
                ("FanArt", ImageType::Logo, "https://f/logo.png"),
                (
                    "The Open Movie Database",
                    ImageType::Primary,
                    "https://o/poster.jpg"
                ),
            ]
        );
        // TMDB's sizes/ratings/languages ride along.
        assert_eq!(all[0].width, Some(1000));
        assert_eq!(all[0].language.as_deref(), Some("en"));
    }

    #[tokio::test]
    async fn refresh_single_item_refreshes_a_person_and_reports_the_verdict() {
        let server = crate::mock_http::MockServer::start(vec![
            (
                "/search/person",
                r#"{"results":[{"id":287,"name":"Brad Pitt","profile_path":"/bp.jpg"}]}"#.to_owned(),
            ),
            (
                "/person/287",
                r#"{"id":287,"name":"Brad Pitt","biography":"An actor.","birthday":"1963-12-18","place_of_birth":"Shawnee, Oklahoma, USA","images":{"profiles":[]}}"#.to_owned(),
            ),
        ])
        .await;
        let tmdb = Arc::new(crate::tmdb::TmdbClient::new().with_base_url(&server.base_url));
        let store = Arc::new(RecordingStore::default());
        let item_id = Uuid::new_v4();
        let mut person = row("Person", "Brad Pitt");
        person.id = item_id.to_string();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let items = Arc::new(FakeItems {
            rows: HashMap::from([(item_id, person)]),
            seen: tx,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_images(tmdb, items)
            .with_image_store(
                Arc::clone(&store) as Arc<dyn ferrofin_traits::persistence::ItemPersistenceService>,
                std::env::temp_dir(),
            );

        // The "refresh people" task's call: default options = fetch missing.
        let update = mgr
            .refresh_single_item(item_id, &MetadataRefreshOptions::default())
            .await
            .expect("refresh");
        assert_eq!(update, ItemUpdateType::MetadataDownload);

        {
            let saved = store.saved.lock().expect("lock");
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0].overview.as_deref(), Some("An actor."));
            assert_eq!(
                saved[0].production_locations.as_deref(),
                Some("Shawnee, Oklahoma, USA")
            );
            assert_eq!(
                saved[0].premiere_date.map(|d| d.to_rfc3339()),
                Some("1963-12-18T00:00:00+00:00".to_owned())
            );
            assert!(saved[0].date_last_refreshed.is_some(), "refresh stamped");
        }
        // The resolved TMDB id is persisted, so the next refresh skips the search.
        assert!(store.upserted.lock().expect("lock").contains(&(
            item_id,
            "Tmdb".to_owned(),
            "287".to_owned()
        )));

        // A validation-only pass fetches nothing and changes nothing.
        let none = mgr
            .refresh_single_item(
                item_id,
                &MetadataRefreshOptions {
                    metadata_refresh_mode: MetadataRefreshMode::ValidationOnly,
                    image_refresh_mode: MetadataRefreshMode::ValidationOnly,
                    ..MetadataRefreshOptions::default()
                },
            )
            .await
            .expect("refresh");
        assert_eq!(none, ItemUpdateType::None);
    }

    #[test]
    fn a_person_resolves_to_a_person_target() {
        use super::refresh_target_of;
        match refresh_target_of(&row("Person", "Brad Pitt"), None) {
            Some(super::RefreshTarget::Person { name }) => assert_eq!(name, "Brad Pitt"),
            other => panic!(
                "expected a Person target, got {}",
                target_name(other.as_ref())
            ),
        }
        assert!(refresh_target_of(&row("Person", ""), None).is_none());
    }

    #[tokio::test]
    async fn queue_refresh_returns_immediately_and_runs_in_background() {
        // A MusicAlbum short-circuits before any network, so the spawned refresh
        // completes offline; the repo channel proves it actually ran.
        let item_id = Uuid::new_v4();
        let mut album = row("Audio.MusicAlbum", "OK Computer");
        album.id = item_id.to_string();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let items = Arc::new(FakeItems {
            rows: HashMap::from([(item_id, album)]),
            seen: tx,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_images(Arc::new(crate::tmdb::TmdbClient::new()), items);

        mgr.queue_refresh(
            item_id,
            &MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
                ..MetadataRefreshOptions::default()
            },
            RefreshPriority::High,
        )
        .await
        .expect("enqueue accepted");

        // The spawned task looked the item up — the refresh ran to completion.
        let seen = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("background refresh ran")
            .expect("channel open");
        assert_eq!(seen, item_id);
    }

    #[tokio::test]
    async fn queue_refresh_swallows_refresh_errors() {
        // The item is missing → the spawned refresh errors NotFound; the enqueue
        // still reports Ok and the error is only logged.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let items = Arc::new(FakeItems {
            rows: HashMap::new(),
            seen: tx,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_images(Arc::new(crate::tmdb::TmdbClient::new()), items);
        mgr.queue_refresh(
            Uuid::new_v4(),
            &MetadataRefreshOptions::default(),
            RefreshPriority::Low,
        )
        .await
        .expect("enqueue accepted despite the failing refresh");
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("background refresh attempted the lookup");
    }

    #[tokio::test]
    async fn resolve_refresh_target_fetches_the_parent_series_row() {
        // The async wrapper hops `series_id` → the repo for seasons; a movie
        // resolves without touching the repo at all.
        let series_id = Uuid::new_v4();
        let mut series = row("TV.Series", "Severance");
        series.id = series_id.to_string();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let items: Arc<dyn ferrofin_traits::persistence::ItemRepository> = Arc::new(FakeItems {
            rows: HashMap::from([(series_id, series)]),
            seen: tx,
        });
        let mgr = LocalProviderManager::default();

        let mut season = row("TV.Season", "Season 1");
        season.series_id = Some(series_id.to_string());
        season.index_number = Some(1);
        let target = mgr
            .resolve_refresh_target(&items, &season)
            .await
            .expect("resolves");
        assert!(matches!(target, Some(super::RefreshTarget::Season { .. })));
        assert_eq!(rx.try_recv().ok(), Some(series_id), "series row fetched");

        let movie = row("Movies.Movie", "Solaris");
        let target = mgr
            .resolve_refresh_target(&items, &movie)
            .await
            .expect("resolves");
        assert!(matches!(target, Some(super::RefreshTarget::Title { .. })));
        assert!(rx.try_recv().is_err(), "movies never hit the repo");
    }

    #[tokio::test]
    async fn apply_tv_slice_applies_metadata_per_refresh_mode() {
        let mgr = LocalProviderManager::default();
        let opts = MetadataRefreshOptions {
            metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
            image_refresh_mode: MetadataRefreshMode::None,
            replace_all_metadata: true,
            replace_all_images: false,
            ..MetadataRefreshOptions::default()
        };
        let mut entity = BaseItemEntity::default();
        mgr.apply_tv_slice(
            &mut entity,
            Uuid::new_v4(),
            Some("Ep 3"),
            Some("Plot."),
            Some("https://example.invalid/still.jpg"),
            &opts,
        )
        .await
        .expect("slice applies");
        assert_eq!(entity.name.as_deref(), Some("Ep 3"));
        assert_eq!(entity.overview.as_deref(), Some("Plot."));

        // ValidationOnly touches nothing.
        let mut untouched = BaseItemEntity::default();
        mgr.apply_tv_slice(
            &mut untouched,
            Uuid::new_v4(),
            Some("X"),
            Some("Y"),
            None,
            &MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::ValidationOnly,
                ..MetadataRefreshOptions::default()
            },
        )
        .await
        .expect("slice no-ops");
        assert_eq!(untouched.name, None);
    }

    #[test]
    fn refresh_target_resolves_season_and_episode_via_parent_series() {
        use super::{RefreshTarget, refresh_target_of};

        let mut series = row("TV.Series", "Severance");
        series.production_year = Some(2022);

        let mut season = row("TV.Season", "Season 2");
        season.series_id = Some(series.id.clone());
        season.index_number = Some(2);
        match refresh_target_of(&season, Some(&series)) {
            Some(RefreshTarget::Season {
                series_id,
                series_name,
                series_year,
                season_number,
            }) => {
                assert_eq!(series_id, Uuid::parse_str(&series.id).ok());
                assert_eq!(series_name, "Severance");
                assert_eq!(series_year, Some(2022));
                assert_eq!(season_number, 2);
            }
            other => panic!(
                "expected a Season target, got {}",
                target_name(other.as_ref())
            ),
        }

        let mut episode = row("TV.Episode", "Hello, Ms. Cobel");
        episode.series_id = Some(series.id.clone());
        episode.parent_index_number = Some(1);
        episode.index_number = Some(3);
        match refresh_target_of(&episode, Some(&series)) {
            Some(RefreshTarget::Episode {
                series_name,
                season_number,
                episode_number,
                ..
            }) => {
                assert_eq!(series_name, "Severance");
                assert_eq!(season_number, 1);
                assert_eq!(episode_number, 3);
            }
            other => panic!(
                "expected an Episode target, got {}",
                target_name(other.as_ref())
            ),
        }
    }

    /// A debug label for a [`super::RefreshTarget`] in test panics.
    fn target_name(target: Option<&super::RefreshTarget>) -> &'static str {
        match target {
            Some(super::RefreshTarget::Title { .. }) => "Title",
            Some(super::RefreshTarget::Season { .. }) => "Season",
            Some(super::RefreshTarget::Episode { .. }) => "Episode",
            Some(super::RefreshTarget::BoxSet { .. }) => "BoxSet",
            Some(super::RefreshTarget::Person { .. }) => "Person",
            None => "None",
        }
    }

    #[test]
    fn a_box_set_resolves_to_a_collection_target() {
        use super::refresh_target_of;
        match refresh_target_of(&row("Movies.BoxSet", "The Matrix Collection"), None) {
            Some(super::RefreshTarget::BoxSet { name }) => {
                assert_eq!(name, "The Matrix Collection");
            }
            other => panic!(
                "expected a BoxSet target, got {}",
                target_name(other.as_ref())
            ),
        }
        // An unnamed box set has nothing to search for.
        let mut unnamed = row("Movies.BoxSet", "");
        unnamed.name = None;
        assert!(refresh_target_of(&unnamed, None).is_none());
    }

    #[tokio::test]
    async fn box_set_remote_images_come_from_the_tmdb_collection() {
        // "Choose Image" on a box set searches TMDB's collections, then lists
        // that collection's artwork — TMDB's own pick first.
        let search = r#"{"results":[{"id":2344,"name":"The Matrix Collection",
                         "poster_path":"/c.jpg","overview":"Neo."}]}"#;
        let collection = r#"{"id":2344,"name":"The Matrix Collection","overview":"Neo.",
            "poster_path":"/pick.jpg","backdrop_path":"/back.jpg",
            "images":{"posters":[{"file_path":"/alt.jpg"}],"backdrops":[]}}"#;
        let server = crate::mock_http::MockServer::start(vec![
            ("/search/collection", search.to_owned()),
            ("/collection/", collection.to_owned()),
        ])
        .await;
        let item_id = Uuid::new_v4();
        let mut boxset = row("Movies.BoxSet", "The Matrix Collection");
        boxset.id = item_id.to_string();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let items = Arc::new(FakeItems {
            rows: HashMap::from([(item_id, boxset)]),
            seen: tx,
        });
        let tmdb = Arc::new(crate::tmdb::TmdbClient::new().with_base_url(&server.base_url));
        let mgr = LocalProviderManager::default().with_remote_images(tmdb, items);

        let images = mgr
            .get_available_remote_images(item_id, &RemoteImageQuery::default())
            .await
            .expect("images");
        let urls: Vec<_> = images.iter().filter_map(|i| i.url.as_deref()).collect();
        assert_eq!(
            urls,
            [
                "https://image.tmdb.org/t/p/original/pick.jpg",
                "https://image.tmdb.org/t/p/original/back.jpg",
                "https://image.tmdb.org/t/p/original/alt.jpg",
            ]
        );
        assert_eq!(images[0].type_, ImageType::Primary);
        assert_eq!(images[1].type_, ImageType::Backdrop);

        // The query's type filter still applies.
        let posters = mgr
            .get_available_remote_images(
                item_id,
                &RemoteImageQuery {
                    image_type: Some(ImageType::Backdrop),
                    ..RemoteImageQuery::default()
                },
            )
            .await
            .expect("images");
        assert_eq!(posters.len(), 1);
        assert_eq!(posters[0].type_, ImageType::Backdrop);
    }

    #[tokio::test]
    async fn the_box_set_identify_provider_returns_collection_candidates() {
        let search = r#"{"results":[{"id":2344,"name":"The Matrix Collection",
                         "poster_path":"/c.jpg","overview":"Neo."}]}"#;
        let server =
            crate::mock_http::MockServer::start(vec![("/search/collection", search.to_owned())])
                .await;
        let tmdb = Arc::new(crate::tmdb::TmdbClient::new().with_base_url(&server.base_url));
        let provider = super::TmdbBoxSetSearchProvider::new(tmdb);
        assert!(provider.supports(BaseItemKind::BoxSet));
        assert!(!provider.supports(BaseItemKind::Movie));
        let results = provider
            .get_search_results(&named_request(BaseItemKind::BoxSet, "Matrix"))
            .await
            .expect("results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name.as_deref(), Some("The Matrix Collection"));
        // The box set carries `Tmdb`, as C# `TmdbBoxSetProvider` sets it —
        // `TmdbCollection` is a *movie*'s pointer at its collection.
        assert_eq!(results[0].provider_ids.as_ref().unwrap()["Tmdb"], "2344");
    }

    #[test]
    fn refresh_target_skips_unsupported_kinds_and_broken_links() {
        use super::refresh_target_of;

        // Music has no provider — the faithful skip.
        assert!(refresh_target_of(&row("Audio.MusicAlbum", "OK Computer"), None).is_none());
        // A season with no resolvable parent series row.
        let mut orphan = row("TV.Season", "Season 1");
        orphan.index_number = Some(1);
        assert!(refresh_target_of(&orphan, None).is_none());
        // An episode missing its season/episode numbers.
        let series = row("TV.Series", "Severance");
        let mut unnumbered = row("TV.Episode", "Mystery");
        unnumbered.series_id = Some(series.id.clone());
        assert!(refresh_target_of(&unnumbered, Some(&series)).is_none());
        // A movie resolves to a Title target (control case).
        assert!(matches!(
            refresh_target_of(&row("Movies.Movie", "Solaris"), None),
            Some(super::RefreshTarget::Title { .. })
        ));
    }

    #[test]
    fn apply_name_overview_fills_and_replaces() {
        use super::apply_name_overview;

        let mut entity = BaseItemEntity::default();
        apply_name_overview(&mut entity, Some("Ep 1"), Some("Plot."), false);
        assert_eq!(entity.name.as_deref(), Some("Ep 1"));
        assert_eq!(entity.overview.as_deref(), Some("Plot."));

        // Existing values are kept without replace…
        apply_name_overview(&mut entity, Some("New"), Some("New plot."), false);
        assert_eq!(entity.name.as_deref(), Some("Ep 1"));
        // …and overwritten with it.
        apply_name_overview(&mut entity, Some("New"), Some("New plot."), true);
        assert_eq!(entity.name.as_deref(), Some("New"));
        assert_eq!(entity.overview.as_deref(), Some("New plot."));

        // A missing TMDB value never clears an existing one.
        apply_name_overview(&mut entity, None, None, true);
        assert_eq!(entity.name.as_deref(), Some("New"));
    }

    /// C# `ProviderManager.CanRefreshMetadata` ("If locked only allow local
    /// providers") and `CanRefreshImages` (locked refuses unless the mode is
    /// `FullRefresh`). Without this gate the dashboard's "Refresh metadata"
    /// re-downloaded TMDB artwork onto an item the user had locked.
    #[tokio::test]
    async fn a_locked_item_refuses_remote_metadata_and_non_full_images() {
        let mgr = LocalProviderManager::new(Vec::new());
        let locked = BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
            is_locked: true,
            ..Default::default()
        };
        let gated = mgr.gated_options(&locked, &Opts::default()).await;
        assert_eq!(gated.metadata_refresh_mode, Mode::None);
        assert_eq!(gated.image_refresh_mode, Mode::None);

        // FullRefresh still bypasses the IsLocked half for IMAGES only.
        let full = Opts {
            metadata_refresh_mode: Mode::FullRefresh,
            image_refresh_mode: Mode::FullRefresh,
            ..Opts::default()
        };
        let gated = mgr.gated_options(&locked, &full).await;
        assert_eq!(gated.metadata_refresh_mode, Mode::None);
        assert_eq!(gated.image_refresh_mode, Mode::FullRefresh);

        // An unlocked item in a library with no saved options is untouched.
        let open = BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
            ..Default::default()
        };
        let gated = mgr.gated_options(&open, &Opts::default()).await;
        assert_eq!(gated.metadata_refresh_mode, Mode::Default);
        assert_eq!(gated.image_refresh_mode, Mode::Default);
    }
}
