//! Batch 4 — `ItemLookupController` remote metadata search + apply.
//!
//! Exercises the ten routes wired in `handlers::item_lookup`: the nine
//! `POST /Items/RemoteSearch/{kind}` searches and
//! `POST /Items/RemoteSearch/Apply/{itemId}`.
//!
//! A provider-backed manager proves the typed query is deserialized,
//! collapsed to the object-safe request (base fields plus the type-specific
//! album-artist / song-info / artist / series-name extras), and its results
//! are returned with the provider name stamped on. `Apply` resolves the item
//! (`404` when absent) and runs the library's Identify refresh
//! (`LibraryManager::refresh_full_item`) with the chosen result bound into
//! the refresh options; what that refresh covers is the library's.

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

/// One `update_item_provider_ids` call: the item and the ids it was given.
type IdsSet = (Uuid, Vec<(String, String)>);

/// A library that resolves only [`ITEM_ID`]; every other id is absent. The
/// Apply route's writes to the library and the Identify refreshes it was
/// asked for are recorded.
#[derive(Default)]
struct OneItemLibrary {
    /// `update_item_provider_ids` calls.
    ids_set: std::sync::Mutex<Vec<IdsSet>>,
    /// `refresh_full_item` calls.
    refreshes: std::sync::Mutex<Vec<(Uuid, MetadataRefreshOptions)>>,
    /// The scanner is stopped: the refresh reports it did not finish.
    stopped: bool,
    /// The scan cannot reach the item: the refresh is refused.
    unreachable: bool,
    /// How long a refresh takes before it records itself.
    refresh_delay: std::time::Duration,
    /// The culture the last refresh ran under.
    refresh_culture: std::sync::Mutex<Option<String>>,
}

#[async_trait]
impl LibraryManager for OneItemLibrary {
    async fn is_item_visible_standalone(
        &self,
        _item: &ferrofin_db::entities::base_items::BaseItemEntity,
        _user: &ferrofin_db::entities::users::UserEntity,
    ) -> Result<bool, ferrofin_traits::error::ServiceError> {
        Ok(true)
    }
    async fn is_item_visible(
        &self,
        _item: &ferrofin_db::entities::base_items::BaseItemEntity,
        _user: &ferrofin_db::entities::users::UserEntity,
    ) -> Result<bool, ferrofin_traits::error::ServiceError> {
        Ok(true)
    }

    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((id == ITEM_ID).then(|| minimal_base_item(ITEM_ID, "The Matrix", "Movie")))
    }
    async fn update_item_provider_ids(
        &self,
        item_id: Uuid,
        provider_ids: &[(String, String)],
    ) -> Result<(), ServiceError> {
        self.ids_set
            .lock()
            .unwrap()
            .push((item_id, provider_ids.to_vec()));
        Ok(())
    }
    async fn refresh_full_item(
        &self,
        item: &BaseItemEntity,
        options: &MetadataRefreshOptions,
    ) -> Result<bool, ServiceError> {
        if self.unreachable {
            return Err(ServiceError::conflict(
                "the item is not under any library location",
            ));
        }
        tokio::time::sleep(self.refresh_delay).await;
        *self.refresh_culture.lock().unwrap() = Some(ferrofin_util::current_culture::capture());
        self.refreshes
            .lock()
            .unwrap()
            .push((Uuid::parse_str(&item.id).expect("id"), options.clone()));
        Ok(!self.stopped)
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

/// A provider manager returning a fixed remote-search hit. Identify's
/// refresh is the library's, so it is never asked to refresh.
#[derive(Default)]
struct SearchProviders;

#[async_trait]
impl ProviderManager for SearchProviders {
    async fn remote_search(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        // Echo the searched name so the test can assert the query was decoded,
        // and the type-specific extras (`album artists | song count | artists |
        // series name`) so the seam is proven to carry them.
        Ok(vec![RemoteSearchResult {
            name: request.search_info.name.clone(),
            overview: Some(format!(
                "{}|{}|{}|{}",
                request.album_artists.join(","),
                request.song_infos.len(),
                request.artists.join(","),
                request.series_name.clone().unwrap_or_default()
            )),
            provider_ids: request.artist_provider_ids.clone(),
            search_provider_name: Some("TheMovieDb".to_owned()),
            ..RemoteSearchResult::default()
        }])
    }

    async fn refresh_full_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!("Identify's refresh is the library's")
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
    state_over(&Arc::new(OneItemLibrary::default()))
}

/// The elevated [`AppState`] over `library`.
fn state_over(library: &Arc<OneItemLibrary>) -> AppState {
    elevated_state_with_library_and_providers(
        Arc::clone(library) as Arc<dyn LibraryManager>,
        Arc::new(SearchProviders),
    )
}

/// The plain-user [`AppState`], for proving which routes an ordinary account
/// may still reach.
fn user_state() -> AppState {
    authed_state_with_library_and_providers(
        Arc::new(OneItemLibrary::default()),
        Arc::new(SearchProviders),
    )
}

/// Sends one request through the real router, returning `(status, body bytes)`.
async fn send(method: &str, uri: &str, body: Body) -> (StatusCode, Vec<u8>) {
    send_to(state(), method, uri, body).await
}

/// Sends one request through the real router over `state`.
async fn send_to(state: AppState, method: &str, uri: &str, body: Body) -> (StatusCode, Vec<u8>) {
    let router = create_router(state);
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

/// The type-specific lookup fields cross the object-safe seam: an album's
/// artists / artist provider ids / song infos, a music video's artists, a
/// book's series name.
#[tokio::test]
async fn typed_lookup_extras_cross_the_seam() {
    let album = r#"{"SearchInfo":{"Name":"Kind of Blue","AlbumArtists":["Miles Davis"],
        "ArtistProviderIds":{"MusicBrainzArtist":"mb-artist"},
        "SongInfos":[{"Name":"So What"},{"Name":"Blue in Green"}]}}"#;
    let (status, bytes) = send("POST", "/Items/RemoteSearch/MusicAlbum", Body::from(album)).await;
    assert_eq!(status, StatusCode::OK);
    let results: Vec<RemoteSearchResult> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(results[0].overview.as_deref(), Some("Miles Davis|2||"));
    assert_eq!(
        results[0].provider_ids.as_ref().unwrap()["MusicBrainzArtist"],
        "mb-artist"
    );

    let artist = r#"{"SearchInfo":{"Name":"Miles Davis","SongInfos":[{"Name":"So What"}]}}"#;
    let (_, bytes) = send(
        "POST",
        "/Items/RemoteSearch/MusicArtist",
        Body::from(artist),
    )
    .await;
    let results: Vec<RemoteSearchResult> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(results[0].overview.as_deref(), Some("|1||"));

    let video = r#"{"SearchInfo":{"Name":"Thriller","Artists":["Michael Jackson"]}}"#;
    let (_, bytes) = send("POST", "/Items/RemoteSearch/MusicVideo", Body::from(video)).await;
    let results: Vec<RemoteSearchResult> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(results[0].overview.as_deref(), Some("|0|Michael Jackson|"));

    let book = r#"{"SearchInfo":{"Name":"Mort","SeriesName":"Discworld"}}"#;
    let (_, bytes) = send("POST", "/Items/RemoteSearch/Book", Body::from(book)).await;
    let results: Vec<RemoteSearchResult> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(results[0].overview.as_deref(), Some("|0||Discworld"));
}

/// The one Identify refresh `library` was asked for.
fn the_refresh(library: &OneItemLibrary) -> (Uuid, MetadataRefreshOptions) {
    let refreshes = library.refreshes.lock().unwrap().clone();
    assert_eq!(refreshes.len(), 1, "{refreshes:?}");
    refreshes[0].clone()
}

/// Apply hands the library's Identify refresh the item and a full,
/// replace-all refresh with the chosen result bound into it, and returns
/// `204` once it has run.
#[tokio::test]
async fn apply_refreshes_existing_item() {
    let library = Arc::new(OneItemLibrary::default());
    let uri = format!("/Items/RemoteSearch/Apply/{ITEM_ID}");
    let body =
        Body::from(r#"{"Name":"The Matrix","ProductionYear":1999,"ProviderIds":{"Tmdb":"603"}}"#);
    let (status, bytes) = send_to(state_over(&library), "POST", &uri, body).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(bytes.is_empty(), "{bytes:?}");

    let (item_id, options) = the_refresh(&library);
    assert_eq!(item_id, ITEM_ID);
    assert_eq!(
        options.metadata_refresh_mode,
        ferrofin_traits::providers::MetadataRefreshMode::FullRefresh
    );
    assert_eq!(
        options.image_refresh_mode,
        ferrofin_traits::providers::MetadataRefreshMode::FullRefresh
    );
    assert!(options.replace_all_metadata && options.remove_old_metadata);
    assert!(
        options.replace_all_images,
        "replaceAllImages defaults to true"
    );
    let chosen = options
        .search_result
        .expect("the chosen result rides along");
    assert_eq!(chosen.name.as_deref(), Some("The Matrix"));
    assert_eq!(chosen.production_year, Some(1999));
    assert_eq!(chosen.provider_ids.unwrap()["Tmdb"], "603");
}

/// `replaceAllImages=false` reaches the refresh, and the chosen ids ride in
/// the refresh's save: the handler writes no provider ids of its own, so a
/// failed refresh leaves no half-applied ids.
#[tokio::test]
async fn apply_hands_the_refresh_the_image_choice_and_the_chosen_ids() {
    let library = Arc::new(OneItemLibrary::default());
    let uri = format!("/Items/RemoteSearch/Apply/{ITEM_ID}?replaceAllImages=false");
    let body = Body::from(
        r#"{"Name":"Heat","ProductionYear":1995,"SearchProviderName":"TheTVDB",
            "ProviderIds":{"Tmdb":"949","Imdb":""}}"#,
    );
    let (status, _) = send_to(state_over(&library), "POST", &uri, body).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        library.ids_set.lock().unwrap().is_empty(),
        "the ids ride in the refresh's save"
    );
    let (_, options) = the_refresh(&library);
    assert!(!options.replace_all_images);
    let chosen = options.search_result.as_ref().expect("the chosen result");
    assert_eq!(chosen.name.as_deref(), Some("Heat"));
    assert_eq!(chosen.search_provider_name.as_deref(), Some("TheTVDB"));
}

/// A refresh stopped before it finished (the scanner is shutting down) is a
/// `503`, never a silent `204` that applied nothing.
#[tokio::test]
async fn apply_when_the_scanner_is_stopped_is_503() {
    let library = Arc::new(OneItemLibrary {
        stopped: true,
        ..OneItemLibrary::default()
    });
    let uri = format!("/Items/RemoteSearch/Apply/{ITEM_ID}");
    let body = Body::from(r#"{"Name":"Heat","ProviderIds":{"Tmdb":"949"}}"#);
    let (status, _) = send_to(state_over(&library), "POST", &uri, body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        library.ids_set.lock().unwrap().is_empty(),
        "nothing written"
    );
}

/// An item the scan cannot reach (its library's folder was removed or
/// moved) is refused by the library's refresh: a `409`, not a silent `204`.
#[tokio::test]
async fn apply_on_an_item_the_scan_cannot_reach_is_409() {
    let library = Arc::new(OneItemLibrary {
        unreachable: true,
        ..OneItemLibrary::default()
    });
    let uri = format!("/Items/RemoteSearch/Apply/{ITEM_ID}");
    let body = Body::from(r#"{"Name":"Heat","ProviderIds":{"Tmdb":"949"}}"#);
    let (status, _) = send_to(state_over(&library), "POST", &uri, body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(library.refreshes.lock().unwrap().is_empty());
    assert!(library.ids_set.lock().unwrap().is_empty());
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

/// Apply on the all-zero id is a `400`: upstream's `GetItemById(Guid.Empty)`
/// throws `ArgumentException`, which the exception middleware answers with
/// `400`, and nothing is refreshed.
#[tokio::test]
async fn apply_on_the_empty_id_is_400() {
    let library = Arc::new(OneItemLibrary::default());
    let uri = format!("/Items/RemoteSearch/Apply/{}", Uuid::nil());
    let (status, _) = send_to(state_over(&library), "POST", &uri, Body::from("{}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        library.refreshes.lock().unwrap().is_empty(),
        "nothing refreshed"
    );
}

/// A client that hangs up during Apply does not stop the refresh: upstream
/// runs `RefreshFullItem` with `CancellationToken.None`. The refresh, on a
/// task of its own, keeps the request's culture.
#[tokio::test]
async fn apply_finishes_after_the_client_hangs_up() {
    let library = Arc::new(OneItemLibrary {
        refresh_delay: std::time::Duration::from_millis(200),
        ..OneItemLibrary::default()
    });
    let state = state_over(&library)
        // A culture no CI host runs under, so the system locale cannot pass.
        .with_request_culture("sv")
        .expect("culture");
    let request = create_router(state).oneshot(
        Request::builder()
            .method("POST")
            .uri(format!("/Items/RemoteSearch/Apply/{ITEM_ID}"))
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    );
    // The client gives up long before the refresh ends: its request future
    // is dropped.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), request)
            .await
            .is_err(),
        "the request outlives the client's 20 ms patience"
    );
    let finished = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while library.refreshes.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(finished.is_ok(), "the refresh ran to its end");
    assert_eq!(
        library.refresh_culture.lock().unwrap().as_deref(),
        Some("sv")
    );
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
