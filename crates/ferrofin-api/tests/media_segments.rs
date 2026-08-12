//! Media segments handler tests: segment query/filtering + the Intro Skipper
//! plugin SegmentEditor route.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate and return canned data. Managers
//! a given handler never touches reuse the `test_support` panic fakes.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeAppHost, FakeCollections, FakeConfig, FakeDto, FakeLyrics, FakeMediaSegments,
    FakeMediaSources, FakeMusic, FakePlaylists, FakeQuickConnect, FakeSearch, FakeSessions,
    FakeSimilarItems, FakeSubtitles, FakeSystem, FakeTrickplay, FakeUserData, FakeUserViews,
    minimal_base_item,
};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::media_segments::{MediaSegmentManager, MediaSegmentProviderInfo};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{
    AuthorizationInfo, DeleteOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::stubs::LyricManager;
use ferrofin_traits::subtitles::SubtitleManager;
use ferrofin_traits::trickplay::TrickplayManager;
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x00B1_0000);
const ITEM_ID: Uuid = Uuid::from_u128(0x00B1_0001);

/// A minimal authenticated user for the stubs.
fn user() -> UserEntity {
    UserEntity {
        id: USER_ID.to_string(),
        audio_language_preference: None,
        authentication_provider_id: String::new(),
        cast_receiver_id: None,
        display_collections_view: false,
        display_missing_episodes: false,
        enable_auto_login: false,
        enable_local_password: false,
        enable_next_episode_auto_play: false,
        enable_user_preference_access: false,
        hide_played_in_latest: false,
        internal_id: 0,
        invalid_login_attempt_count: 0,
        last_activity_date: None,
        last_login_date: None,
        login_attempts_before_lockout: None,
        max_active_sessions: 0,
        max_parental_rating_score: None,
        max_parental_rating_sub_score: None,
        must_update_password: false,
        password: Some("hashed".to_owned()),
        password_reset_provider_id: String::new(),
        play_default_audio_track: false,
        remember_audio_selections: false,
        remember_subtitle_selections: false,
        remote_client_bitrate_limit: None,
        row_version: 0,
        subtitle_language_preference: None,
        subtitle_mode: 0,
        sync_play_access: 0,
        username: "bob".to_owned(),
    }
}

/// An [`AuthService`]/[`AuthorizationContext`] that authenticates as [`USER_ID`].
struct OkAuth;

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            token: Some("tok".into()),
            is_api_key: false,
            user: Some(user()),
            is_authenticated: true,
            ..Default::default()
        })
    }
}

#[async_trait]
impl AuthorizationContext for OkAuth {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            token: Some("tok".into()),
            user: Some(user()),
            is_authenticated: true,
            ..Default::default()
        })
    }
}

/// A [`LibraryManager`] resolving [`ITEM_ID`] to an item and everything else to
/// `None` (so the handlers' `404` branch is exercised).
struct OneItemLibrary;

#[async_trait]
impl LibraryManager for OneItemLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        if id == ITEM_ID {
            Ok(Some(minimal_base_item(id, "The Item", "Movie")))
        } else {
            Ok(None)
        }
    }
    async fn query_items(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<ferrofin_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_item_ids(&self, _query: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_item_list(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_latest_item_list(
        &self,
        _query: &InternalItemsQuery,
        _collection_type: ferrofin_model::data::CollectionType,
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
    ) -> Result<Vec<ferrofin_db::entities::base_items::PeopleEntity>, ServiceError> {
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
    ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("unused")
    }
    async fn get_studios(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("unused")
    }
    async fn get_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("unused")
    }
    async fn get_music_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("unused")
    }
    async fn get_album_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("unused")
    }
    async fn get_query_filters_legacy(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: ferrofin_model::entities::MediaStreamType,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!("unused")
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
}

/// A [`MediaSegmentManager`] returning one canned intro segment for any item.
struct OneSegment;

#[async_trait]
impl MediaSegmentManager for OneSegment {
    async fn is_type_supported(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn create_segment(
        &self,
        segment: &MediaSegmentDto,
        _segment_provider_id: &str,
    ) -> Result<MediaSegmentDto, ServiceError> {
        Ok(segment.clone())
    }
    async fn delete_segment(&self, _segment_id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn delete_segments(&self, _item_id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_segments(
        &self,
        item_id: Uuid,
        type_filter: Option<&[MediaSegmentType]>,
        _filter_by_provider: bool,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        let seg = MediaSegmentDto {
            id: Uuid::from_u128(9),
            item_id,
            type_: MediaSegmentType::Intro,
            start_ticks: 0,
            end_ticks: 100,
        };
        // Honour the type filter so the filtered test can assert narrowing.
        if let Some(types) = type_filter
            && !types.is_empty()
            && !types.contains(&seg.type_)
        {
            return Ok(Vec::new());
        }
        Ok(vec![seg])
    }
    async fn has_segments(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn get_supported_providers(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<MediaSegmentProviderInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn delete_provider_segments(
        &self,
        _item_id: Uuid,
        _provider_id: &str,
        _type_filter: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// Builds an [`AppState`] over the media-segment manager, defaulting the
/// untouched ones to the shared panic fakes.
#[allow(clippy::too_many_arguments)]
fn state(
    segments: Arc<dyn MediaSegmentManager>,
    trickplay: Arc<dyn TrickplayManager>,
    lyrics: Arc<dyn LyricManager>,
    subtitles: Arc<dyn SubtitleManager>,
) -> AppState {
    AppState::new(
        Arc::new(OneItemLibrary),
        Arc::new(ferrofin_api::test_support::FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(ferrofin_api::test_support::FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(OkAuth),
        Arc::new(OkAuth),
        Arc::new(FakeQuickConnect),
        Arc::new(FakePlaylists),
        Arc::new(FakeCollections),
        Arc::new(ferrofin_api::test_support::FakeTvSeries),
        subtitles,
        lyrics,
        segments,
        trickplay,
        Arc::new(ferrofin_api::test_support::FakeDevices),
        Arc::new(ferrofin_api::test_support::FakeClientEventLogger),
        Arc::new(ferrofin_api::test_support::FakeApiKeys),
        Arc::new(ferrofin_api::test_support::FakeLocalization),
        Arc::new(ferrofin_api::test_support::FakeDisplayPreferences),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(ferrofin_api::test_support::FakeFileSystem),
        Arc::new(ferrofin_api::test_support::FakeTasks),
    )
}

/// Sends an authenticated request and returns `(status, body-bytes)`.
async fn call(app: AppState, method: &str, uri: &str) -> (StatusCode, Vec<u8>) {
    call_with_body(app, method, uri, Body::empty(), None).await
}

async fn call_with_body(
    app: AppState,
    method: &str,
    uri: &str,
    body: Body,
    content_type: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Emby-Token", "tok");
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    let response = create_router(app)
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

#[tokio::test]
async fn media_segments_returns_query_result() {
    let app = state(
        Arc::new(OneSegment),
        Arc::new(FakeTrickplay),
        Arc::new(FakeLyrics),
        Arc::new(FakeSubtitles),
    );
    let (status, body) = call(app, "GET", &format!("/MediaSegments/{ITEM_ID}")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 1);
    assert_eq!(v["Items"][0]["Type"], "Intro");
}

#[tokio::test]
async fn media_segments_type_filter_narrows() {
    let app = state(
        Arc::new(OneSegment),
        Arc::new(FakeTrickplay),
        Arc::new(FakeLyrics),
        Arc::new(FakeSubtitles),
    );
    // The only stored segment is Intro; filtering to Outro yields none.
    let (status, body) = call(
        app,
        "GET",
        &format!("/MediaSegments/{ITEM_ID}?includeSegmentTypes=Outro"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 0);
}

#[tokio::test]
async fn media_segments_accepts_repeated_type_params() {
    let app = state(
        Arc::new(OneSegment),
        Arc::new(FakeTrickplay),
        Arc::new(FakeLyrics),
        Arc::new(FakeSubtitles),
    );
    // The jellyfin SDK sends the filter as a REPEATED query parameter
    // (`?includeSegmentTypes=Intro&includeSegmentTypes=Outro`); a duplicate-key
    // 400 here breaks every playback's segment fetch (no skip button).
    let (status, body) = call(
        app,
        "GET",
        &format!("/MediaSegments/{ITEM_ID}?includeSegmentTypes=Intro&includeSegmentTypes=Outro"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 1);
    assert_eq!(v["Items"][0]["Type"], "Intro");
}

#[tokio::test]
async fn media_segments_missing_item_is_404() {
    let app = state(
        Arc::new(OneSegment),
        Arc::new(FakeTrickplay),
        Arc::new(FakeLyrics),
        Arc::new(FakeSubtitles),
    );
    let (status, _) = call(
        app,
        "GET",
        &format!("/MediaSegments/{}", Uuid::from_u128(7)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plugin_segment_editor_route_is_implemented() {
    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(FakeLyrics),
        Arc::new(FakeSubtitles),
    );
    // The Intro Skipper SegmentEditor create route (`POST /MediaSegmentsApi/{id}`)
    // used to sit on the 501 stub; it is now a real handler (see
    // `handlers::intro_skipper`). Target an unknown item so the handler resolves
    // it to `404` up front — proving the route is implemented (not `501`) without
    // reaching the fake segment store.
    let unknown = Uuid::from_u128(0x00B1_DEAD);
    let (status, _) = call_with_body(
        app,
        "POST",
        &format!("/MediaSegmentsApi/{unknown}?providerId=IntroSkipper"),
        Body::from(r#"{"Type":"Intro","StartTicks":0,"EndTicks":100}"#),
        Some("application/json"),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::NOT_IMPLEMENTED,
        "SegmentEditor route should be implemented, not 501"
    );
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown item should be 404");
}
