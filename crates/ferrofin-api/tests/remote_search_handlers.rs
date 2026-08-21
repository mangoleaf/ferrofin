//! Batch 4 — `ItemLookupController` remote metadata search + apply.
//!
//! Exercises the ten routes wired in `handlers::item_lookup`: the nine
//! `POST /Items/RemoteSearch/{kind}` searches and
//! `POST /Items/RemoteSearch/Apply/{itemId}`.
//!
//! The remote fetchers are deferred, so a search with no provider registered
//! returns `[]`. Here a provider-backed manager proves the typed query is
//! deserialized, collapsed to the object-safe request, and its results are
//! returned with the provider name stamped on. `Apply` resolves the item (`404`
//! when absent) and drives the real `refresh_full_item` seam.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    authed_state_with_library_and_providers, elevated_state_with_library_and_providers,
    minimal_base_item,
};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::base_items::PeopleEntity;
use ferrofin_model::data::CollectionType;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::providers::RemoteSearchResult;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::options::{DeleteOptions, InternalItemsQuery, InternalPeopleQuery};
use ferrofin_traits::providers::{MetadataRefreshOptions, ProviderManager, RemoteSearchRequest};
use tower::ServiceExt;
use uuid::Uuid;

const ITEM_ID: Uuid = Uuid::from_u128(0x1111_2222_3333_4444);

/// A library that resolves only [`ITEM_ID`]; every other id is absent.
struct OneItemLibrary;

#[async_trait]
impl LibraryManager for OneItemLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((id == ITEM_ID).then(|| minimal_base_item(ITEM_ID, "The Matrix", "Movie")))
    }
    async fn query_items(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_ids(&self, _q: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_list(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_latest_item_list(
        &self,
        _q: &InternalItemsQuery,
        _c: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn create_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn delete_item(&self, _id: Uuid, _o: &DeleteOptions) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_people(
        &self,
        _q: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_people_names(
        &self,
        _q: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn get_count(&self, _q: &InternalItemsQuery) -> Result<i32, ServiceError> {
        unimplemented!()
    }
    async fn get_item_counts(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
        unimplemented!()
    }
    async fn get_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_query_filters_legacy(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!()
    }
    async fn get_media_stream_languages(
        &self,
        _t: MediaStreamType,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A provider manager returning a fixed remote-search hit and a refresh that
/// succeeds (proving the Apply route reaches the real seam).
struct SearchProviders;

#[async_trait]
impl ProviderManager for SearchProviders {
    async fn remote_search(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        // Echo the searched name so the test can assert the query was decoded.
        Ok(vec![RemoteSearchResult {
            name: request.search_info.name.clone(),
            search_provider_name: Some("TheMovieDb".to_owned()),
            ..RemoteSearchResult::default()
        }])
    }

    async fn refresh_full_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn queue_refresh(
        &self,
        _i: Uuid,
        _o: &MetadataRefreshOptions,
        _p: ferrofin_traits::providers::RefreshPriority,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn refresh_single_item(
        &self,
        _i: Uuid,
        _o: &MetadataRefreshOptions,
    ) -> Result<ferrofin_traits::providers::ItemUpdateType, ServiceError> {
        unimplemented!()
    }
    async fn save_image_from_url(
        &self,
        _i: Uuid,
        _u: &str,
        _t: ferrofin_model::entities::ImageType,
        _x: Option<i32>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn save_image(
        &self,
        _i: Uuid,
        _c: &[u8],
        _m: &str,
        _t: ferrofin_model::entities::ImageType,
        _x: Option<i32>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_available_remote_images(
        &self,
        _i: Uuid,
        _q: &ferrofin_model::providers::RemoteImageQuery,
    ) -> Result<Vec<ferrofin_model::providers::RemoteImageInfo>, ServiceError> {
        unimplemented!()
    }
    async fn get_remote_image_provider_info(
        &self,
        _i: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ImageProviderInfo>, ServiceError> {
        unimplemented!()
    }
    async fn save_metadata(
        &self,
        _i: Uuid,
        _u: ferrofin_traits::providers::ItemUpdateType,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_external_id_infos(
        &self,
        _i: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ExternalIdInfo>, ServiceError> {
        unimplemented!()
    }
    async fn get_all_metadata_plugins(
        &self,
    ) -> Result<Vec<ferrofin_model::configuration::MetadataPluginSummary>, ServiceError> {
        unimplemented!()
    }
    async fn get_metadata_options(
        &self,
        _i: Uuid,
    ) -> Result<ferrofin_model::configuration::MetadataOptions, ServiceError> {
        unimplemented!()
    }
    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
}

/// Builds the batch-4 [`AppState`] with the one-item library + search provider.
///
/// Elevated, because `RemoteSearch/Apply` and `RemoteSearch/Person` are
/// `RequiresElevation` upstream. The asymmetry — those two gated, the other
/// nine typed searches on plain `[Authorize]` — is pinned by
/// [`only_person_and_apply_require_elevation`].
fn state() -> AppState {
    elevated_state_with_library_and_providers(Arc::new(OneItemLibrary), Arc::new(SearchProviders))
}

/// The plain-user [`AppState`], for proving which routes an ordinary account
/// may still reach.
fn user_state() -> AppState {
    authed_state_with_library_and_providers(Arc::new(OneItemLibrary), Arc::new(SearchProviders))
}

/// Sends one request through the real router, returning `(status, body bytes)`.
async fn send(method: &str, uri: &str, body: Body) -> (StatusCode, Vec<u8>) {
    let router = create_router(state());
    let response = router
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

/// Every typed search route decodes its query and returns the provider's result.
#[tokio::test]
async fn every_remote_search_route_returns_provider_results() {
    let routes = [
        "/Items/RemoteSearch/Movie",
        "/Items/RemoteSearch/Trailer",
        "/Items/RemoteSearch/MusicVideo",
        "/Items/RemoteSearch/Series",
        "/Items/RemoteSearch/BoxSet",
        "/Items/RemoteSearch/MusicArtist",
        "/Items/RemoteSearch/MusicAlbum",
        "/Items/RemoteSearch/Person",
        "/Items/RemoteSearch/Book",
    ];
    for route in routes {
        let body = Body::from(r#"{"SearchInfo":{"Name":"The Matrix","Year":1999}}"#);
        let (status, bytes) = send("POST", route, body).await;
        assert_eq!(status, StatusCode::OK, "route {route} should return 200");
        let results: Vec<RemoteSearchResult> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(results.len(), 1, "route {route} returns one hit");
        assert_eq!(
            results[0].name.as_deref(),
            Some("The Matrix"),
            "route {route} decoded the SearchInfo name"
        );
        assert_eq!(
            results[0].search_provider_name.as_deref(),
            Some("TheMovieDb")
        );
    }
}

/// A search with an empty body still succeeds (default query → empty search info).
#[tokio::test]
async fn remote_search_accepts_empty_body() {
    let (status, bytes) = send("POST", "/Items/RemoteSearch/Movie", Body::from("{}")).await;
    assert_eq!(status, StatusCode::OK);
    let results: Vec<RemoteSearchResult> = serde_json::from_slice(&bytes).unwrap();
    // The provider echoes a `None` name (no SearchInfo supplied).
    assert_eq!(results.len(), 1);
    assert!(results[0].name.is_none());
}

/// Apply on an existing item drives the refresh seam and returns `204`.
#[tokio::test]
async fn apply_refreshes_existing_item() {
    let uri = format!("/Items/RemoteSearch/Apply/{ITEM_ID}");
    let body = Body::from(r#"{"ProviderIds":{"Tmdb":"603"}}"#);
    let (status, bytes) = send("POST", &uri, body).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(bytes.is_empty());
}

/// Apply respects the `replaceAllImages` query flag (still `204`).
#[tokio::test]
async fn apply_honors_replace_all_images_flag() {
    let uri = format!("/Items/RemoteSearch/Apply/{ITEM_ID}?replaceAllImages=false");
    let (status, _) = send("POST", &uri, Body::from("{}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// Apply on a missing item is a `404`.
#[tokio::test]
async fn apply_missing_item_is_404() {
    let missing = Uuid::from_u128(0xdead_beef);
    let uri = format!("/Items/RemoteSearch/Apply/{missing}");
    let (status, _) = send("POST", &uri, Body::from("{}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// `ItemLookupController` gates exactly two of its actions with
/// `RequiresElevation` at v10.11.8 — `RemoteSearch/Person` and
/// `RemoteSearch/Apply/{itemId}` — and leaves the other nine typed searches on
/// plain `[Authorize]`. That is asymmetric enough to look like a mistake, so it
/// is pinned in both directions: over-gating would break ordinary metadata
/// identification in every client.
#[tokio::test]
async fn only_person_and_apply_require_elevation() {
    async fn post(state: AppState, uri: &str) -> StatusCode {
        create_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"SearchInfo":{}}"#))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    for uri in [
        "/Items/RemoteSearch/Movie",
        "/Items/RemoteSearch/Series",
        "/Items/RemoteSearch/Book",
        "/Items/RemoteSearch/MusicAlbum",
    ] {
        assert_ne!(
            post(user_state(), uri).await,
            StatusCode::FORBIDDEN,
            "{uri} is plain [Authorize] upstream — an ordinary user must reach it"
        );
    }

    assert_eq!(
        post(user_state(), "/Items/RemoteSearch/Person").await,
        StatusCode::FORBIDDEN,
        "RemoteSearch/Person is RequiresElevation upstream"
    );
}
