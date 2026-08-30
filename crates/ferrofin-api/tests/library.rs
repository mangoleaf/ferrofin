//! Library — filesystem-monitor change-report webhooks + library scan trigger.
//!
//! Drives the five `LibraryController` external-source change-report routes
//! through the real router, against a [`RecordingLibrary`] (filters a preset item
//! set by the same `IncludeItemTypes` + exact-provider-id criteria the handlers
//! push into the query) and a [`RecordingMonitor`] (captures every reported
//! path), asserting the ported status codes and that exactly the right paths are
//! reported to [`LibraryMonitor`]:
//!
//! - `POST /Library/Series/Added` / `Updated` — by TVDB id.
//! - `POST /Library/Movies/Added` / `Updated` — by IMDb id (preferred) or TMDb id.
//! - `POST /Library/Media/Updated` — by request-body paths.
//! - `POST /Library/Refresh` — queues a full library scan.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_api::create_router;
use ferrofin_api::test_support::{
    RecordingTasks, authed_state_with_library_and_monitor, elevated_state_with_library_and_monitor,
    elevated_state_with_library_monitor_and_tasks,
};
use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_model::data::CollectionType;
use ferrofin_model::dto::ItemCounts;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, LibraryMonitor};
use ferrofin_traits::options::{DeleteOptions, InternalItemsQuery, InternalPeopleQuery};
use ferrofin_traits::persistence::ItemWithCounts;
use tower::ServiceExt;
use uuid::Uuid;

/// The permissive-auth token the always-authenticating state accepts.
const TOKEN: &str = "valid";

/// Builds a minimal [`BaseItemEntity`] of `kind` at `path`; every other column is
/// a neutral zero value.
fn item(kind: &str, path: &str) -> BaseItemEntity {
    BaseItemEntity {
        id: Uuid::new_v4().to_string(),
        album: None,
        album_artists: None,
        artists: None,
        audio: None,
        channel_id: None,
        clean_name: None,
        community_rating: None,
        critic_rating: None,
        custom_rating: None,
        data: None,
        date_created: None,
        date_last_media_added: None,
        date_last_refreshed: None,
        date_last_saved: None,
        date_modified: None,
        end_date: None,
        episode_title: None,
        external_id: None,
        external_series_id: None,
        external_service_id: None,
        extra_type: None,
        forced_sort_name: None,
        genres: None,
        height: None,
        index_number: None,
        inherited_parental_rating_sub_value: None,
        inherited_parental_rating_value: None,
        is_folder: false,
        is_in_mixed_folder: false,
        is_locked: false,
        is_movie: false,
        is_repeat: false,
        is_series: false,
        is_virtual_item: false,
        lufs: None,
        media_type: None,
        name: Some("title".to_owned()),
        normalization_gain: None,
        official_rating: None,
        extra_ids: None,
        original_title: None,
        overview: None,
        owner_id: None,
        parent_id: None,
        parent_index_number: None,
        path: Some(path.to_owned()),
        preferred_metadata_country_code: None,
        preferred_metadata_language: None,
        premiere_date: None,
        presentation_unique_key: None,
        primary_version_id: None,
        production_locations: None,
        production_year: None,
        run_time_ticks: None,
        season_id: None,
        season_name: None,
        series_id: None,
        series_name: None,
        series_presentation_unique_key: None,
        show_id: None,
        size: None,
        sort_name: None,
        start_date: None,
        studios: None,
        tagline: None,
        tags: None,
        top_parent_id: None,
        total_bitrate: None,
        type_: kind.to_owned(),
        unrated_type: None,
        width: None,
    }
}

/// One seeded item: its entity plus its external provider ids.
struct SeededItem {
    entity: BaseItemEntity,
    provider_ids: Vec<(&'static str, &'static str)>,
}

/// A [`LibraryManager`] that filters a preset item set exactly as the real query
/// would: by `IncludeItemTypes` and by `AnyProviderIdEquals` (case-insensitive on
/// key and value). Every other method is unused by these webhooks.
struct RecordingLibrary {
    items: Vec<SeededItem>,
}

#[async_trait]
impl LibraryManager for RecordingLibrary {
    async fn get_item_list(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let type_names: Vec<String> = query
            .include_item_types
            .iter()
            .map(|k| format!("{k:?}"))
            .collect();
        Ok(self
            .items
            .iter()
            .filter(|it| type_names.iter().any(|t| t == &it.entity.type_))
            .filter(|it| {
                query.any_provider_id_equals.is_empty()
                    || query.any_provider_id_equals.iter().any(|(k, v)| {
                        it.provider_ids.iter().any(|(pk, pv)| {
                            pk.eq_ignore_ascii_case(k) && pv.eq_ignore_ascii_case(v)
                        })
                    })
            })
            .map(|it| it.entity.clone())
            .collect())
    }

    async fn get_item_by_id(&self, _id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        unimplemented!("unused")
    }
    async fn query_items(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_item_ids(&self, _query: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_latest_item_list(
        &self,
        _query: &InternalItemsQuery,
        _collection_type: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("unused")
    }
    async fn create_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn update_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn delete_item(&self, _id: Uuid, _options: &DeleteOptions) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn get_people(
        &self,
        _query: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_people_names(
        &self,
        _query: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_count(&self, _query: &InternalItemsQuery) -> Result<i32, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_item_counts(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<ItemCounts, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_studios(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_music_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_album_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_query_filters_legacy(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: MediaStreamType,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!("unused")
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        // Merged from batch14's `StubLibrary` so `POST /Library/Refresh` succeeds
        // under this harness (the change-report webhooks never call it).
        Ok(())
    }
}

/// A [`LibraryMonitor`] that records every reported path.
#[derive(Default)]
struct RecordingMonitor {
    reported: Mutex<Vec<String>>,
}

#[async_trait]
impl LibraryMonitor for RecordingMonitor {
    async fn start(&self) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn report_file_system_change_beginning(&self, _path: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn report_file_system_change_complete(
        &self,
        _path: &str,
        _refresh_path: bool,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn report_file_system_changed(&self, path: &str) -> Result<(), ServiceError> {
        self.reported.lock().unwrap().push(path.to_owned());
        Ok(())
    }
}

/// The seeded catalogue used by the selection tests: two series (one TVDB id),
/// two movies (one IMDb + TMDb id, one only TMDb).
fn seeded_items() -> Vec<SeededItem> {
    vec![
        SeededItem {
            entity: item("Series", "/tv/The Wire"),
            provider_ids: vec![("Tvdb", "79126")],
        },
        SeededItem {
            entity: item("Series", "/tv/Other Show"),
            provider_ids: vec![("Tvdb", "999")],
        },
        SeededItem {
            entity: item("Movie", "/movies/Heat"),
            provider_ids: vec![("Imdb", "tt0113277"), ("Tmdb", "949")],
        },
        SeededItem {
            entity: item("Movie", "/movies/Only Tmdb"),
            provider_ids: vec![("Tmdb", "500")],
        },
    ]
}

/// Builds the router over the recording doubles, returning the shared monitor.
fn router_with(items: Vec<SeededItem>) -> (axum::Router, Arc<RecordingMonitor>) {
    router_with_auth(items, false)
}

/// `router_with` for a caller satisfying `RequiresElevation` — needed by
/// `POST /Library/Refresh`, which is admin-only upstream.
fn router_with_auth(
    items: Vec<SeededItem>,
    elevated: bool,
) -> (axum::Router, Arc<RecordingMonitor>) {
    let monitor = Arc::new(RecordingMonitor::default());
    let library: Arc<RecordingLibrary> = Arc::new(RecordingLibrary { items });
    let state = if elevated {
        elevated_state_with_library_and_monitor(library, monitor.clone())
    } else {
        authed_state_with_library_and_monitor(library, monitor.clone())
    };
    (create_router(state), monitor)
}

/// The paths the monitor observed, in report order.
fn reported(monitor: &RecordingMonitor) -> Vec<String> {
    monitor.reported.lock().unwrap().clone()
}

async fn post(router: axum::Router, uri: &str, body: Option<&str>) -> StatusCode {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("X-Emby-Token", TOKEN);
    let body = if let Some(json) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(json.to_owned())
    } else {
        Body::empty()
    };
    router
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn series_updated_reports_matching_tvdb_path() {
    let (router, monitor) = router_with(seeded_items());
    let status = post(router, "/Library/Series/Updated?tvdbId=79126", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(reported(&monitor), vec!["/tv/The Wire".to_owned()]);
}

#[tokio::test]
async fn series_added_uses_the_same_handler() {
    let (router, monitor) = router_with(seeded_items());
    let status = post(router, "/Library/Series/Added?tvdbId=79126", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(reported(&monitor), vec!["/tv/The Wire".to_owned()]);
}

#[tokio::test]
async fn series_updated_without_tvdb_reports_nothing() {
    let (router, monitor) = router_with(seeded_items());
    let status = post(router, "/Library/Series/Updated", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(reported(&monitor).is_empty());
}

#[tokio::test]
async fn series_updated_unknown_tvdb_reports_nothing() {
    let (router, monitor) = router_with(seeded_items());
    let status = post(router, "/Library/Series/Updated?tvdbId=nomatch", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(reported(&monitor).is_empty());
}

#[tokio::test]
async fn movies_updated_prefers_imdb_over_tmdb() {
    let (router, monitor) = router_with(seeded_items());
    // Heat has IMDb tt0113277 AND TMDb 949; the "Only Tmdb" movie has TMDb 500.
    // Supplying both: IMDb wins, so only Heat is reported.
    let status = post(
        router,
        "/Library/Movies/Updated?imdbId=tt0113277&tmdbId=500",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(reported(&monitor), vec!["/movies/Heat".to_owned()]);
}

#[tokio::test]
async fn movies_updated_falls_back_to_tmdb() {
    let (router, monitor) = router_with(seeded_items());
    let status = post(router, "/Library/Movies/Updated?tmdbId=500", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(reported(&monitor), vec!["/movies/Only Tmdb".to_owned()]);
}

#[tokio::test]
async fn movies_added_without_any_id_reports_nothing() {
    let (router, monitor) = router_with(seeded_items());
    let status = post(router, "/Library/Movies/Added", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(reported(&monitor).is_empty());
}

#[tokio::test]
async fn media_updated_reports_each_body_path() {
    let (router, monitor) = router_with(seeded_items());
    let body = r#"{"Updates":[{"Path":"/a/x.mkv","UpdateType":"Modified"},{"Path":"/b/y.mkv"}]}"#;
    let status = post(router, "/Library/Media/Updated", Some(body)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        reported(&monitor),
        vec!["/a/x.mkv".to_owned(), "/b/y.mkv".to_owned()]
    );
}

#[tokio::test]
async fn media_updated_rejects_null_path() {
    let (router, monitor) = router_with(seeded_items());
    let body = r#"{"Updates":[{"Path":"/a/x.mkv"},{"UpdateType":"Deleted"}]}"#;
    let status = post(router, "/Library/Media/Updated", Some(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // NOT atomic, and deliberately so. C# is
    //   foreach (var item in dto.Updates)
    //       _libraryMonitor.ReportFileSystemChanged(
    //           item.Path ?? throw new ArgumentException("Item path can't be null."));
    // (v10.11.8 LibraryController.cs:648-651) — the throw is INSIDE the loop, so
    // every path ahead of the null one has already been reported. Asserting an
    // empty list here would pin a batch-atomicity Jellyfin does not have.
    assert_eq!(reported(&monitor), vec!["/a/x.mkv".to_owned()]);
}

#[tokio::test]
async fn media_updated_empty_batch_is_noop() {
    let (router, monitor) = router_with(seeded_items());
    let status = post(router, "/Library/Media/Updated", Some(r#"{"Updates":[]}"#)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(reported(&monitor).is_empty());
}

#[tokio::test]
async fn series_updated_empty_tvdb_reports_nothing() {
    // An empty (but present) `tvdbId` is treated as "no id" — no series match.
    let (router, monitor) = router_with(seeded_items());
    let status = post(router, "/Library/Series/Updated?tvdbId=", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(reported(&monitor).is_empty());
}

#[tokio::test]
async fn movies_updated_empty_imdb_falls_back_to_tmdb() {
    // An empty `imdbId` is ignored, so the non-empty `tmdbId` selects the movie.
    let (router, monitor) = router_with(seeded_items());
    let status = post(router, "/Library/Movies/Updated?imdbId=&tmdbId=500", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(reported(&monitor), vec!["/movies/Only Tmdb".to_owned()]);
}

#[tokio::test]
async fn refresh_library_returns_204() {
    // `POST /Library/Refresh` answers 204 — and does it by driving the task
    // registry, which is the whole observable effect of the route. C#'s
    // `LibraryManager.ValidateMediaLibrary` is, verbatim,
    // `_taskManager.CancelIfRunningAndQueue<RefreshMediaLibraryTask>()`
    // (v10.11.8 LibraryManager.cs:1117-1123): cancel first so a scan already in
    // flight restarts rather than being rejected, then queue. That routing is
    // what puts "Scan Media Library" into GET /ScheduledTasks as Running, which
    // is what jellyfin-web's "Scan all libraries" button reports to the operator.
    // Asserting only the 204 would pass just as well against a no-op handler.
    let tasks = Arc::new(RecordingTasks::default());
    let monitor = Arc::new(RecordingMonitor::default());
    let library: Arc<RecordingLibrary> = Arc::new(RecordingLibrary {
        items: seeded_items(),
    });
    let state = elevated_state_with_library_monitor_and_tasks(
        library,
        monitor,
        Arc::clone(&tasks) as Arc<dyn ferrofin_traits::tasks::TaskManager>,
    );
    let router = ferrofin_api::create_router(state);
    let status = post(router, "/Library/Refresh", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        *tasks.cancelled.lock().expect("cancelled"),
        vec!["RefreshLibrary".to_owned()]
    );
    assert_eq!(
        *tasks.started.lock().expect("started"),
        vec!["RefreshLibrary".to_owned()]
    );
}
