//! Remote similarity providers — port of Jellyfin 12.0's
//! `TmdbMovieSimilarProvider`, `TmdbSeriesSimilarProvider` and
//! `ListenBrainzSimilarArtistProvider`.
//!
//! Each returns lightweight
//! [`SimilarItemReference`](ferrofin_traits::library::SimilarItemReference)s
//! keyed by an external provider id; the similar-items manager resolves them
//! against the library and merges them with the local scorer's results. A
//! provider a library has not ticked never runs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::library::{
    RemoteSimilarItemsProvider, SimilarItemReference, SimilarItemsQuery,
};

use crate::listenbrainz::ListenBrainzClient;
use crate::tmdb::{TmdbClient, TmdbKind};

/// How many `/similar` pages a TMDB lookup will walk before giving up.
///
/// C# walks until `page > totalPages` or enough local matches resolve; a real
/// library rarely matches beyond the first page or two, and each page is a
/// request, so the walk is bounded here.
const MAX_TMDB_SIMILAR_PAGES: i32 = 3;

/// How many days a TMDB similarity result is cached — C#
/// `Plugins/Tmdb/Configuration/PluginConfiguration.SimilarItemsCacheDays`.
pub const TMDB_SIMILAR_CACHE_DAYS: i64 = 7;

/// TMDB's "similar titles" for a movie or series.
pub struct TmdbSimilarProvider {
    tmdb: Arc<TmdbClient>,
    kind: TmdbKind,
    supported: BaseItemKind,
    cache_days: i64,
}

impl TmdbSimilarProvider {
    /// A TMDB similarity provider for `kind`, caching its references for
    /// `cache_days` (Jellyfin's `SimilarItemsCacheDays`, default 7; `0`
    /// disables caching).
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>, kind: TmdbKind, cache_days: i64) -> Self {
        let supported = match kind {
            TmdbKind::Movie => BaseItemKind::Movie,
            TmdbKind::Series => BaseItemKind::Series,
        };
        Self {
            tmdb,
            kind,
            supported,
            cache_days,
        }
    }
}

#[async_trait]
impl RemoteSimilarItemsProvider for TmdbSimilarProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "TheMovieDb"
    }

    fn supports(&self, item_kind: BaseItemKind) -> bool {
        item_kind == self.supported
    }

    fn cache_duration(&self) -> Option<Duration> {
        (self.cache_days > 0).then(|| {
            Duration::from_secs(u64::try_from(self.cache_days).unwrap_or(0) * 24 * 60 * 60)
        })
    }

    async fn get_similar_items(
        &self,
        _seed: &BaseItemEntity,
        seed_provider_ids: &HashMap<String, String>,
        _query: &SimilarItemsQuery,
    ) -> Vec<SimilarItemReference> {
        // The seed's own TMDB id keys the lookup; without one there is nothing
        // to ask TMDB about (C# `yield break`).
        let Some(tmdb_id) =
            provider_id(seed_provider_ids, "Tmdb").and_then(|v| v.parse::<i64>().ok())
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut page = 1;
        let mut total_pages = 1;
        while page <= total_pages.min(MAX_TMDB_SIMILAR_PAGES) {
            let (ids, reported) = self.tmdb.similar_page(self.kind, tmdb_id, page).await;
            if ids.is_empty() {
                break;
            }
            total_pages = reported.max(1);
            out.extend(ids.into_iter().map(|id| SimilarItemReference {
                provider_name: "Tmdb".to_owned(),
                provider_id: id.to_string(),
                score: None,
            }));
            page += 1;
        }
        out
    }
}

/// ListenBrainz's similar artists, keyed by MusicBrainz artist id.
pub struct ListenBrainzSimilarArtistProvider {
    client: Arc<ListenBrainzClient>,
}

impl ListenBrainzSimilarArtistProvider {
    /// A similar-artist provider over the ListenBrainz Labs client.
    #[must_use]
    pub fn new(client: Arc<ListenBrainzClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl RemoteSimilarItemsProvider for ListenBrainzSimilarArtistProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "ListenBrainz"
    }

    fn supports(&self, item_kind: BaseItemKind) -> bool {
        item_kind == BaseItemKind::MusicArtist
    }

    fn cache_duration(&self) -> Option<Duration> {
        self.client.cache_duration()
    }

    async fn get_similar_items(
        &self,
        _seed: &BaseItemEntity,
        seed_provider_ids: &HashMap<String, String>,
        _query: &SimilarItemsQuery,
    ) -> Vec<SimilarItemReference> {
        let Some(mbid) = provider_id(seed_provider_ids, "MusicBrainzArtist") else {
            return Vec::new();
        };
        self.client
            .similar_artists(&mbid)
            .await
            .into_iter()
            .map(|id| SimilarItemReference {
                provider_name: "MusicBrainzArtist".to_owned(),
                provider_id: id,
                score: None,
            })
            .collect()
    }
}

/// One of the seed's provider ids, matched case-insensitively (Jellyfin's
/// `TryGetProviderId`, whose dictionary is ordinal-ignore-case).
fn provider_id(ids: &HashMap<String, String>, key: &str) -> Option<String> {
    ids.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal seed row of `kind`.
    fn row(kind: &str) -> BaseItemEntity {
        BaseItemEntity {
            id: uuid::Uuid::new_v4().to_string(),
            type_: format!("MediaBrowser.Controller.Entities.{kind}"),
            ..BaseItemEntity::default()
        }
    }

    /// A provider-id map.
    fn ids(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn provider_ids_are_read_case_insensitively() {
        let map = ids(&[("tmdb", "27205")]);
        assert_eq!(provider_id(&map, "Tmdb").as_deref(), Some("27205"));
        assert_eq!(provider_id(&map, "Imdb"), None);
        assert_eq!(provider_id(&HashMap::new(), "Tmdb"), None);
        // A blank id is no id.
        assert_eq!(provider_id(&ids(&[("Tmdb", "  ")]), "Tmdb"), None);
    }

    #[tokio::test]
    async fn tmdb_similar_yields_nothing_without_a_tmdb_id() {
        let provider = TmdbSimilarProvider::new(Arc::new(TmdbClient::new()), TmdbKind::Movie, 7);
        assert!(provider.supports(BaseItemKind::Movie));
        assert!(!provider.supports(BaseItemKind::Series));
        // No id → no request, so this resolves without touching the network.
        let refs = provider
            .get_similar_items(
                &row("Movies.Movie"),
                &HashMap::new(),
                &SimilarItemsQuery::default(),
            )
            .await;
        assert!(refs.is_empty());
    }

    #[test]
    fn a_zero_cache_day_setting_disables_caching() {
        let tmdb = Arc::new(TmdbClient::new());
        assert_eq!(
            TmdbSimilarProvider::new(Arc::clone(&tmdb), TmdbKind::Movie, 0).cache_duration(),
            None
        );
        assert_eq!(
            TmdbSimilarProvider::new(tmdb, TmdbKind::Movie, 7)
                .cache_duration()
                .map(|d| d.as_secs()),
            Some(7 * 24 * 60 * 60)
        );
    }

    #[tokio::test]
    async fn listenbrainz_yields_nothing_without_a_musicbrainz_id() {
        let provider =
            ListenBrainzSimilarArtistProvider::new(Arc::new(ListenBrainzClient::default()));
        assert!(provider.supports(BaseItemKind::MusicArtist));
        assert!(!provider.supports(BaseItemKind::MusicAlbum));
        let refs = provider
            .get_similar_items(
                &row("Audio.MusicArtist"),
                &HashMap::new(),
                &SimilarItemsQuery::default(),
            )
            .await;
        assert!(refs.is_empty());
    }
}
