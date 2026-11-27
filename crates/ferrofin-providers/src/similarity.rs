//! Remote similarity providers — port of Jellyfin 12.0's
//! `TmdbMovieSimilarProvider`, `TmdbSeriesSimilarProvider` and
//! `ListenBrainzSimilarArtistProvider`.
//!
//! Each returns lightweight
//! [`SimilarItemReference`]s
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
    RemoteSimilarItemsProvider, SimilarItemReference, SimilarItemsPage, SimilarItemsQuery,
};

use crate::listenbrainz::ListenBrainzClient;
use crate::tmdb::{TmdbClient, TmdbKind};

/// A plugin's `SimilarItemsCacheDays` as a provider's `CacheDuration`:
/// `days > 0 ? TimeSpan.FromDays(days) : null`.
fn cache_days(days: i32) -> Option<Duration> {
    u64::try_from(days)
        .ok()
        .filter(|days| *days > 0)
        .map(|days| Duration::from_secs(days * 24 * 60 * 60))
}

/// TMDB's recommendations for a movie or series (Jellyfin 12 reads them, not
/// TMDB's keyword-based "similar" list).
pub struct TmdbSimilarProvider {
    tmdb: Arc<TmdbClient>,
    kind: TmdbKind,
    supported: BaseItemKind,
}

impl TmdbSimilarProvider {
    /// A TMDB similarity provider for `kind`, caching its references for the
    /// TMDb plugin's `SimilarItemsCacheDays`.
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
impl RemoteSimilarItemsProvider for TmdbSimilarProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "TheMovieDb"
    }

    fn supports(&self, item_kind: BaseItemKind) -> bool {
        item_kind == self.supported
    }

    /// `TmdbMovieSimilarProvider.CacheDuration` /
    /// `TmdbSeriesSimilarProvider.CacheDuration`, read each time.
    async fn cache_duration(&self) -> Option<Duration> {
        cache_days(self.tmdb.settings().await.similar_items_cache_days)
    }

    async fn similar_items_page(
        &self,
        _seed: &BaseItemEntity,
        seed_provider_ids: &HashMap<String, String>,
        _query: &SimilarItemsQuery,
        page: i32,
    ) -> SimilarItemsPage {
        // The seed's own TMDB id keys the lookup; without one there is nothing
        // to ask TMDB about (C# `yield break`).
        let Some(tmdb_id) =
            provider_id(seed_provider_ids, "Tmdb").and_then(|v| v.parse::<i64>().ok())
        else {
            return SimilarItemsPage::default();
        };
        // One page of the C# provider's `while (page <= totalPages)` walk; the
        // manager decides whether there is a next one.
        let (ids, total_pages) = self
            .tmdb
            .recommendations_page(self.kind, tmdb_id, page)
            .await;
        SimilarItemsPage {
            references: ids
                .into_iter()
                .map(|id| SimilarItemReference {
                    provider_name: "Tmdb".to_owned(),
                    provider_id: id.to_string(),
                    score: None,
                })
                .collect(),
            total_pages,
        }
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

    /// `ListenBrainzSimilarArtistProvider.CacheDuration`, read each time.
    async fn cache_duration(&self) -> Option<Duration> {
        cache_days(self.client.settings().await.cache_days)
    }

    async fn similar_items_page(
        &self,
        _seed: &BaseItemEntity,
        seed_provider_ids: &HashMap<String, String>,
        _query: &SimilarItemsQuery,
        page: i32,
    ) -> SimilarItemsPage {
        // ListenBrainz answers the whole list at once: it is page 1, of 1.
        let Some(mbid) = provider_id(seed_provider_ids, "MusicBrainzArtist").filter(|_| page == 1)
        else {
            return SimilarItemsPage::default();
        };
        SimilarItemsPage {
            references: self
                .client
                .similar_artists(&mbid)
                .await
                .into_iter()
                .map(|id| SimilarItemReference {
                    provider_name: "MusicBrainzArtist".to_owned(),
                    provider_id: id,
                    score: None,
                })
                .collect(),
            total_pages: 1,
        }
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
        let provider = TmdbSimilarProvider::new(Arc::new(TmdbClient::new()), TmdbKind::Movie);
        assert!(provider.supports(BaseItemKind::Movie));
        assert!(!provider.supports(BaseItemKind::Series));
        // No id → no request, so this resolves without touching the network.
        let page = provider
            .similar_items_page(
                &row("Movies.Movie"),
                &HashMap::new(),
                &SimilarItemsQuery::default(),
                1,
            )
            .await;
        assert!(page.references.is_empty(), "{:?}", page.references);
    }

    /// One call is one TMDB page: the page the manager asks for, with the
    /// page count TMDB reports, so the manager can stop the walk.
    #[tokio::test]
    async fn tmdb_similar_answers_the_page_it_is_asked_for() {
        let body = r#"{"page":3,"total_pages":16,"results":[{"id":603},{"id":604}]}"#;
        let server = crate::mock_http::MockServer::start(vec![(
            "/movie/27205/recommendations?",
            body.to_owned(),
        )])
        .await;
        let tmdb = Arc::new(TmdbClient::new().with_base_url(&server.base_url));
        let provider = TmdbSimilarProvider::new(tmdb, TmdbKind::Movie);
        let page = provider
            .similar_items_page(
                &row("Movies.Movie"),
                &ids(&[("Tmdb", "27205")]),
                &SimilarItemsQuery::default(),
                3,
            )
            .await;
        let ids: Vec<&str> = page
            .references
            .iter()
            .map(|r| r.provider_id.as_str())
            .collect();
        assert_eq!(ids, ["603", "604"]);
        assert!(page.references.iter().all(|r| r.provider_name == "Tmdb"));
        assert_eq!(page.total_pages, 16);
    }

    /// ListenBrainz has no pages: everything is page 1, and a later page is
    /// empty without a request.
    #[tokio::test]
    async fn listenbrainz_answers_nothing_past_page_one() {
        let provider =
            ListenBrainzSimilarArtistProvider::new(Arc::new(ListenBrainzClient::default()));
        let page = provider
            .similar_items_page(
                &row("Audio.MusicArtist"),
                &ids(&[("MusicBrainzArtist", "a74b1b7f-71a5-4011-9441-d0b5e4122711")]),
                &SimilarItemsQuery::default(),
                2,
            )
            .await;
        assert!(page.references.is_empty(), "{:?}", page.references);
    }

    /// The cache length is each plugin's `SimilarItemsCacheDays`, read when
    /// the manager asks: TMDb's 90 days and ListenBrainz's 14 by default, `0`
    /// (or less) no cache, as upstream's `days > 0` test.
    #[tokio::test]
    async fn each_plugins_cache_days_set_the_cache_duration() {
        const DAY: u64 = 24 * 60 * 60;
        let tmdb = |config: &str| {
            let client = TmdbClient::new();
            client.attach_plugin_manager(crate::plugin_config::tests_support::manager_with(
                crate::builtin_plugins::TMDB.id,
                config.as_bytes().to_vec(),
            ));
            TmdbSimilarProvider::new(Arc::new(client), TmdbKind::Series)
        };
        let listenbrainz = |config: &str| {
            let client = ListenBrainzClient::new();
            client.attach_plugin_manager(crate::plugin_config::tests_support::manager_with(
                crate::builtin_plugins::LISTENBRAINZ.id,
                config.as_bytes().to_vec(),
            ));
            ListenBrainzSimilarArtistProvider::new(Arc::new(client))
        };
        let secs = |d: Option<Duration>| d.map(|d| d.as_secs());
        assert_eq!(secs(tmdb("{}").cache_duration().await), Some(90 * DAY));
        assert_eq!(
            secs(
                tmdb(r#"{"SimilarItemsCacheDays":3}"#)
                    .cache_duration()
                    .await
            ),
            Some(3 * DAY)
        );
        assert_eq!(
            secs(listenbrainz("{}").cache_duration().await),
            Some(14 * DAY)
        );
        assert_eq!(
            secs(
                listenbrainz(r#"{"SimilarItemsCacheDays":365}"#)
                    .cache_duration()
                    .await
            ),
            Some(365 * DAY)
        );
        for off in ["0", "-3"] {
            let config = format!(r#"{{"SimilarItemsCacheDays":{off}}}"#);
            assert_eq!(tmdb(&config).cache_duration().await, None, "{off}");
            assert_eq!(listenbrainz(&config).cache_duration().await, None, "{off}");
        }
        // The largest C# `int` fits.
        assert_eq!(secs(cache_days(i32::MAX)), Some(2_147_483_647 * DAY));
    }

    #[tokio::test]
    async fn listenbrainz_yields_nothing_without_a_musicbrainz_id() {
        let provider =
            ListenBrainzSimilarArtistProvider::new(Arc::new(ListenBrainzClient::default()));
        assert!(provider.supports(BaseItemKind::MusicArtist));
        assert!(!provider.supports(BaseItemKind::MusicAlbum));
        let page = provider
            .similar_items_page(
                &row("Audio.MusicArtist"),
                &HashMap::new(),
                &SimilarItemsQuery::default(),
                1,
            )
            .await;
        assert!(page.references.is_empty(), "{:?}", page.references);
    }
}
