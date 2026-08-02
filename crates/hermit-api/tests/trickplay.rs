//! Trickplay handler tests: playlist manifest + tile file serving.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `hermit-traits` impls that authenticate and return canned data. The
//! trickplay tile route serves a real temp file so the `ServeFile` tail is
//! covered.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeAppHost, FakeCollections, FakeConfig, FakeDto, FakeLyrics, FakeMediaSegments,
    FakeMediaSources, FakeMusic, FakePlaylists, FakeQuickConnect, FakeSearch, FakeSessions,
    FakeSimilarItems, FakeSubtitles, FakeSystem, FakeUserData, FakeUserViews, minimal_base_item,
};
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::users::UserEntity;
use hermit_traits::error::ServiceError;
use hermit_traits::library::LibraryManager;
use hermit_traits::media_segments::MediaSegmentManager;
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::{
    AuthorizationInfo, DeleteOptions, InternalItemsQuery, InternalPeopleQuery,
};
use hermit_traits::stubs::LyricManager;
use hermit_traits::subtitles::SubtitleManager;
use hermit_traits::trickplay::TrickplayManager;
use std::collections::HashMap;
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
        normalized_username: "BOB".to_owned(),
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
            token: Some("tok".to_owned()),
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
            token: Some("tok".to_owned()),
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
    ) -> Result<hermit_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
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
        _collection_type: hermit_model::data::CollectionType,
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
    ) -> Result<Vec<hermit_db::entities::base_items::PeopleEntity>, ServiceError> {
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
    ) -> Result<hermit_model::dto::ItemCounts, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("unused")
    }
    async fn get_studios(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("unused")
    }
    async fn get_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("unused")
    }
    async fn get_music_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("unused")
    }
    async fn get_album_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("unused")
    }
    async fn get_query_filters_legacy(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<hermit_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: hermit_model::entities::MediaStreamType,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!("unused")
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
}

/// A [`TrickplayManager`] returning a canned playlist + a caller-supplied tile
/// path (so the file-serving tail can be exercised against a temp file).
struct CannedTrickplay {
    tile_path: Option<String>,
}

#[async_trait]
impl TrickplayManager for CannedTrickplay {
    async fn refresh_trickplay_data(
        &self,
        _item_id: Uuid,
        _replace: bool,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_trickplay_resolutions(
        &self,
        _item_id: Uuid,
    ) -> Result<HashMap<i32, hermit_db::entities::playback::TrickplayInfoEntity>, ServiceError>
    {
        Ok(HashMap::new())
    }
    async fn get_trickplay_items(
        &self,
        _limit: i32,
        _offset: i32,
    ) -> Result<Vec<hermit_db::entities::playback::TrickplayInfoEntity>, ServiceError> {
        Ok(Vec::new())
    }
    async fn save_trickplay_info(
        &self,
        _info: &hermit_db::entities::playback::TrickplayInfoEntity,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn delete_trickplay_data(&self, _item_id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_trickplay_manifest(
        &self,
        _item_id: Uuid,
    ) -> Result<
        HashMap<String, HashMap<i32, hermit_db::entities::playback::TrickplayInfoEntity>>,
        ServiceError,
    > {
        Ok(HashMap::new())
    }
    async fn get_hls_playlist(
        &self,
        _item_id: Uuid,
        width: i32,
        _api_key: Option<&str>,
    ) -> Result<Option<String>, ServiceError> {
        // Width 320 has a playlist; anything else does not.
        if width == 320 {
            Ok(Some(
                "#EXTM3U\n#EXT-X-IMAGES-ONLY\n#EXT-X-ENDLIST\n".to_owned(),
            ))
        } else {
            Ok(None)
        }
    }
    async fn get_trickplay_tile_path(
        &self,
        _item_id: Uuid,
        _width: i32,
        _index: i32,
    ) -> Result<Option<String>, ServiceError> {
        Ok(self.tile_path.clone())
    }
}

/// Builds an [`AppState`] over the trickplay manager, defaulting the untouched
/// ones to the shared panic fakes.
#[allow(clippy::too_many_arguments)]
fn state(
    segments: Arc<dyn MediaSegmentManager>,
    trickplay: Arc<dyn TrickplayManager>,
    lyrics: Arc<dyn LyricManager>,
    subtitles: Arc<dyn SubtitleManager>,
) -> AppState {
    AppState::new(
        Arc::new(OneItemLibrary),
        Arc::new(hermit_api::test_support::FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(hermit_api::test_support::FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(OkAuth),
        Arc::new(OkAuth),
        Arc::new(FakeQuickConnect),
        Arc::new(FakePlaylists),
        Arc::new(FakeCollections),
        Arc::new(hermit_api::test_support::FakeTvSeries),
        subtitles,
        lyrics,
        segments,
        trickplay,
        Arc::new(hermit_api::test_support::FakeDevices),
        Arc::new(hermit_api::test_support::FakeClientEventLogger),
        Arc::new(hermit_api::test_support::FakeApiKeys),
        Arc::new(hermit_api::test_support::FakeLocalization),
        Arc::new(hermit_api::test_support::FakeDisplayPreferences),
        Arc::new(hermit_api::test_support::FakeActivity),
        Arc::new(hermit_api::test_support::FakeFileSystem),
        Arc::new(hermit_api::test_support::FakeTasks),
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
async fn trickplay_playlist_ok_and_not_found() {
    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(CannedTrickplay { tile_path: None }),
        Arc::new(FakeLyrics),
        Arc::new(FakeSubtitles),
    );
    let (ok, body) = call(
        app.clone(),
        "GET",
        &format!("/Videos/{ITEM_ID}/Trickplay/320/tiles.m3u8"),
    )
    .await;
    assert_eq!(ok, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("#EXTM3U"));

    let (missing, _) = call(
        app,
        "GET",
        &format!("/Videos/{ITEM_ID}/Trickplay/999/tiles.m3u8"),
    )
    .await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn trickplay_tile_serves_file_then_404() {
    // A real temp tile file so the ServeFile tail runs.
    let mut path = std::env::temp_dir();
    path.push(format!("hermit-tile-{}.jpg", Uuid::new_v4()));
    std::fs::write(&path, b"\xff\xd8\xff\xe0JFIF-ish").expect("write tile");

    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(CannedTrickplay {
            tile_path: Some(path.to_string_lossy().into_owned()),
        }),
        Arc::new(FakeLyrics),
        Arc::new(FakeSubtitles),
    );
    let (ok, body) = call(app, "GET", &format!("/Videos/{ITEM_ID}/Trickplay/320/0")).await;
    assert_eq!(ok, StatusCode::OK);
    assert!(body.starts_with(&[0xff, 0xd8]));
    std::fs::remove_file(&path).ok();

    // No tile path resolved → 404.
    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(CannedTrickplay { tile_path: None }),
        Arc::new(FakeLyrics),
        Arc::new(FakeSubtitles),
    );
    let (missing, _) = call(app, "GET", &format!("/Videos/{ITEM_ID}/Trickplay/320/0")).await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
}
