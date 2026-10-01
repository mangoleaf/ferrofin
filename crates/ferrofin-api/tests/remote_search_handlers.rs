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
//! (`404` when absent) and refreshes it with the chosen result bound into the
//! refresh options: an item with no file of its own through the
//! `refresh_full_item` seam, a file item through the item refresh of its
//! path, whose save writes the result's provider ids.

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
use ferrofin_traits::library::{LibraryManager, ScanTarget};
use ferrofin_traits::options::{DeleteOptions, InternalItemsQuery, InternalPeopleQuery};
use ferrofin_traits::providers::{MetadataRefreshOptions, ProviderManager, RemoteSearchRequest};
use tower::ServiceExt;
use uuid::Uuid;

const ITEM_ID: Uuid = Uuid::from_u128(0x1111_2222_3333_4444);

/// One `update_item_provider_ids` call: the item and the ids it was given.
type IdsSet = (Uuid, Vec<(String, String)>);

/// A library that resolves only [`ITEM_ID`]; every other id is absent. The
/// item has no file unless `file` gives it a path in a library, and the
/// Apply route's writes to the library are recorded.
#[derive(Default)]
struct OneItemLibrary {
    /// The item's path and the library holding it.
    file: Option<(String, Uuid)>,
    /// `update_item_provider_ids` calls.
    ids_set: std::sync::Mutex<Vec<IdsSet>>,
    /// `run_refresh_scan` calls.
    scans: std::sync::Mutex<Vec<(ScanTarget, MetadataRefreshOptions)>>,
    /// The scanner is stopped: `run_refresh_scan` runs nothing.
    stopped: bool,
    /// The item is a music artist known only by name (no folder, no
    /// library).
    by_name_artist: bool,
}

#[async_trait]
impl LibraryManager for OneItemLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((id == ITEM_ID).then(|| {
            if self.by_name_artist {
                let mut artist = minimal_base_item(ITEM_ID, "Gil Evans", "MusicArtist");
                "MediaBrowser.Controller.Entities.Audio.MusicArtist".clone_into(&mut artist.type_);
                artist.path = Some("/config/metadata/artists/Gil Evans".to_owned());
                return artist;
            }
            let mut item = minimal_base_item(ITEM_ID, "The Matrix", "Movie");
            if let Some((path, library)) = &self.file {
                item.path = Some(path.clone());
                item.top_parent_id = Some(library.to_string());
            }
            item
        }))
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
    async fn run_refresh_scan(
        &self,
        target: ScanTarget,
        options: &MetadataRefreshOptions,
    ) -> Result<bool, ServiceError> {
        self.scans.lock().unwrap().push((target, options.clone()));
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

/// The last `(item, options)` the Apply route handed to `refresh_full_item`.
type RefreshRecorder = Arc<std::sync::Mutex<Option<(Uuid, MetadataRefreshOptions)>>>;

/// A provider manager returning a fixed remote-search hit and a refresh that
/// succeeds (proving the Apply route reaches the real seam), recording the
/// refresh it was handed.
#[derive(Default)]
struct SearchProviders {
    last_refresh: RefreshRecorder,
}

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
        item_id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        *self.last_refresh.lock().unwrap() = Some((item_id, options.clone()));
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
    state_recording(RefreshRecorder::default())
}

/// The elevated [`AppState`] whose provider manager records refreshes into
/// `recorder`.
fn state_recording(recorder: RefreshRecorder) -> AppState {
    elevated_state_with_library_and_providers(
        Arc::new(OneItemLibrary::default()),
        Arc::new(SearchProviders {
            last_refresh: recorder,
        }),
    )
}

/// The plain-user [`AppState`], for proving which routes an ordinary account
/// may still reach.
fn user_state() -> AppState {
    authed_state_with_library_and_providers(
        Arc::new(OneItemLibrary::default()),
        Arc::new(SearchProviders::default()),
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

/// Apply on an existing item with no file of its own drives the provider
/// manager's refresh seam with the chosen result bound into a full,
/// replace-all refresh, and returns `204`.
#[tokio::test]
async fn apply_refreshes_existing_item() {
    let uri = format!("/Items/RemoteSearch/Apply/{ITEM_ID}");
    let body =
        Body::from(r#"{"Name":"The Matrix","ProductionYear":1999,"ProviderIds":{"Tmdb":"603"}}"#);
    let recorder = RefreshRecorder::default();
    let (status, bytes) = send_to(state_recording(recorder.clone()), "POST", &uri, body).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(bytes.is_empty());

    let (item_id, options) = recorder.lock().unwrap().clone().expect("refresh ran");
    assert_eq!(item_id, ITEM_ID);
    assert_eq!(
        options.metadata_refresh_mode,
        ferrofin_traits::providers::MetadataRefreshMode::FullRefresh
    );
    assert_eq!(
        options.image_refresh_mode,
        ferrofin_traits::providers::MetadataRefreshMode::FullRefresh
    );
    assert!(options.replace_all_metadata);
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

const MATRIX_PATH: &str = "/media/movies/The Matrix (1999)/The Matrix (1999).mkv";

/// The elevated state over a [`OneItemLibrary`] whose item is a file under
/// the `Movies` library at `/media/movies/`.
fn file_item_state(library: &Arc<OneItemLibrary>, recorder: RefreshRecorder) -> AppState {
    let movies = ferrofin_model::entities_media::VirtualFolderInfo {
        name: Some("Movies".to_owned()),
        locations: vec!["/media/movies/".to_owned()],
        ..Default::default()
    };
    elevated_state_with_library_and_providers(
        Arc::clone(library) as Arc<dyn LibraryManager>,
        Arc::new(SearchProviders {
            last_refresh: recorder,
        }),
    )
    .with_virtual_folders(Arc::new(
        ferrofin_api::test_support::FakeVirtualFolders::seeded(vec![movies]),
    ))
}

/// A [`OneItemLibrary`] whose item is [`MATRIX_PATH`].
fn file_library(stopped: bool) -> Arc<OneItemLibrary> {
    Arc::new(OneItemLibrary {
        file: Some((MATRIX_PATH.to_owned(), Uuid::from_u128(0xF1))),
        stopped,
        ..OneItemLibrary::default()
    })
}

/// Apply on a file item refreshes it through the item refresh of its own
/// path — the scan's metadata service skips the NFO, pins every fetcher to
/// the chosen ids and writes them with the rest of its save (so a failed
/// refresh leaves no half-applied ids); the provider manager's refresh is
/// not used, nor is a separate provider-id write. Awaited, like upstream's
/// `RefreshFullItem`.
#[tokio::test]
async fn apply_on_a_file_item_refreshes_it_through_its_scan() {
    let library = file_library(false);
    let recorder = RefreshRecorder::default();
    let state = file_item_state(&library, recorder.clone());
    let uri = format!("/Items/RemoteSearch/Apply/{ITEM_ID}?replaceAllImages=false");
    let body = Body::from(
        r#"{"Name":"Heat","ProductionYear":1995,"SearchProviderName":"TheTVDB",
            "ProviderIds":{"Tmdb":"949","Imdb":""}}"#,
    );
    let (status, _) = send_to(state, "POST", &uri, body).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert!(
        recorder.lock().unwrap().is_none(),
        "the provider manager is not asked"
    );
    assert!(
        library.ids_set.lock().unwrap().is_empty(),
        "the ids ride in the scan's save"
    );
    let scans = library.scans.lock().unwrap().clone();
    assert_eq!(scans.len(), 1);
    let (target, options) = &scans[0];
    assert_eq!(*target, ScanTarget::Items(vec![MATRIX_PATH.to_owned()]));
    assert_eq!(
        options.metadata_refresh_mode,
        ferrofin_traits::providers::MetadataRefreshMode::FullRefresh
    );
    assert!(options.replace_all_metadata && options.remove_old_metadata);
    assert!(!options.replace_all_images);
    let chosen = options.search_result.as_ref().expect("the chosen result");
    assert_eq!(chosen.name.as_deref(), Some("Heat"));
    assert_eq!(chosen.search_provider_name.as_deref(), Some("TheTVDB"));
}

/// Apply on a music artist known only by name refreshes it through the
/// scanner — `ScanTarget::Artist` with no folder, in the priority lane —
/// whose music pass fetches MusicBrainz and TheAudioDB by the chosen id; the
/// provider manager (which has no music provider) is not used. Its metadata
/// path under the config directory is under no library, which used to make
/// this a `409`.
#[tokio::test]
async fn apply_on_a_by_name_artist_refreshes_it_through_the_scanner() {
    let library = Arc::new(OneItemLibrary {
        by_name_artist: true,
        ..OneItemLibrary::default()
    });
    let recorder = RefreshRecorder::default();
    let state = file_item_state(&library, recorder.clone());
    let uri = format!("/Items/RemoteSearch/Apply/{ITEM_ID}");
    let body = Body::from(
        r#"{"Name":"Gil Evans","SearchProviderName":"MusicBrainz",
            "ProviderIds":{"MusicBrainzArtist":"66666666-6666-4666-8666-666666666666"}}"#,
    );
    let (status, _) = send_to(state, "POST", &uri, body).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        recorder.lock().unwrap().is_none(),
        "the provider manager is not asked"
    );
    let scans = library.scans.lock().unwrap().clone();
    assert_eq!(scans.len(), 1);
    let (target, options) = &scans[0];
    assert_eq!(
        *target,
        ScanTarget::Artist {
            id: ITEM_ID,
            path: None,
            folders: Vec::new(),
        }
    );
    assert!(options.replace_all_metadata && options.remove_old_metadata);
    let chosen = options.search_result.as_ref().expect("the chosen result");
    assert_eq!(
        chosen.provider_ids.as_ref().expect("ids")["MusicBrainzArtist"],
        "66666666-6666-4666-8666-666666666666"
    );
}

/// A stopped scanner runs nothing, so Apply is a `503`, never a silent
/// `204` that applied nothing.
#[tokio::test]
async fn apply_when_the_scanner_is_stopped_is_503() {
    let library = file_library(true);
    let state = file_item_state(&library, RefreshRecorder::default());
    let uri = format!("/Items/RemoteSearch/Apply/{ITEM_ID}");
    let body = Body::from(r#"{"Name":"Heat","ProviderIds":{"Tmdb":"949"}}"#);
    let (status, _) = send_to(state, "POST", &uri, body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        library.ids_set.lock().unwrap().is_empty(),
        "nothing written"
    );
}

/// A file item whose path no library location covers any more (the
/// library's folder was removed or moved) cannot be refreshed by the scan,
/// and a provider-only refresh would skip its probe and NFO: a `409`, not a
/// silent `204`.
#[tokio::test]
async fn apply_on_a_file_no_library_reaches_is_409() {
    let library = file_library(false);
    let state = elevated_state_with_library_and_providers(
        Arc::clone(&library) as Arc<dyn LibraryManager>,
        Arc::new(SearchProviders::default()),
    );
    let uri = format!("/Items/RemoteSearch/Apply/{ITEM_ID}");
    let body = Body::from(r#"{"Name":"Heat","ProviderIds":{"Tmdb":"949"}}"#);
    let (status, _) = send_to(state, "POST", &uri, body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(library.scans.lock().unwrap().is_empty());
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
