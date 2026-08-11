//! Streaming/playback handler tests: bitrate-test, live-stream open/close, and
//! item download.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `hermit-traits` impls that authenticate and return canned data. The
//! download route serves a real temp file so the `ServeFile` path is exercised
//! end to end.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeAppHost, FakeCollections, FakeConfig, FakeMusic, FakePlaylists, FakeProviders,
    FakeQuickConnect, FakeSearch, FakeSessions, FakeSimilarItems, FakeSystem, FakeTvSeries,
    FakeUserData, FakeUserViews, minimal_base_item,
};
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::users::UserEntity;
use hermit_model::dto::{MediaSourceInfo, MediaSourceType};
use hermit_model::entities_media::{MediaAttachment, MediaStream};
use hermit_model::media_info::{LiveStreamRequest, MediaProtocol};
use hermit_model::querying::QueryResult;
use hermit_traits::error::ServiceError;
use hermit_traits::library::{LibraryManager, MediaSourceManager, UserManager};
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::{
    AuthorizationInfo, DeleteOptions, InternalItemsQuery, InternalPeopleQuery,
};
use hermit_traits::persistence::ItemWithCounts;
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x00A1_0000);
const ITEM_ID: Uuid = Uuid::from_u128(0x00A1_0001);

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
        username: "alice".to_owned(),
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
            user: Some(user()),
            is_authenticated: true,
            ..AuthorizationInfo::default()
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
            user: Some(user()),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// A [`UserManager`] resolving the fixed authenticated user.
struct OkUsers;

#[async_trait]
impl UserManager for OkUsers {
    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        Ok((id == USER_ID).then(user))
    }
    async fn get_users(&self) -> Result<Vec<UserEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_user_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
    async fn initialize(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_first_user(&self) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_user_by_name(&self, _name: &str) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!()
    }
    async fn rename_user(
        &self,
        _user_id: Uuid,
        _old_name: &str,
        _new_name: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_user(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn create_user(&self, _name: &str) -> Result<UserEntity, ServiceError> {
        unimplemented!()
    }
    async fn delete_user(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn reset_password(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn change_password(
        &self,
        _user_id: Uuid,
        _new_password: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn authenticate_user(
        &self,
        _username: &str,
        _password: &str,
        _remote_endpoint: &str,
        _is_user_session: bool,
    ) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_authentication_providers(
        &self,
    ) -> Result<Vec<hermit_model::dto::NameIdPair>, ServiceError> {
        unimplemented!()
    }
    async fn get_password_reset_providers(
        &self,
    ) -> Result<Vec<hermit_model::dto::NameIdPair>, ServiceError> {
        unimplemented!()
    }
    async fn get_user_dto(
        &self,
        _user: &UserEntity,
        _server_id: Option<String>,
    ) -> Result<hermit_model::dto::UserDto, ServiceError> {
        unimplemented!()
    }
    async fn update_configuration(
        &self,
        _user_id: Uuid,
        _config: &hermit_model::configuration::UserConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_policy(
        &self,
        _user_id: Uuid,
        _policy: &hermit_model::users::UserPolicy,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn clear_profile_image(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`MediaSourceManager`] that serves a static source for a temp file, and
/// records live-stream open/close so the round-trip is observable.
struct StreamSources {
    /// The on-disk path the static source points at.
    path: String,
    /// Ids passed to `open_live_stream`/`close_live_stream`, for assertions.
    opened: Arc<Mutex<Vec<String>>>,
    closed: Arc<Mutex<Vec<String>>>,
}

fn static_source(path: &str) -> MediaSourceInfo {
    MediaSourceInfo {
        id: Some(ITEM_ID.to_string()),
        path: Some(path.to_owned()),
        protocol: MediaProtocol::File,
        type_: MediaSourceType::Default,
        supports_direct_play: true,
        supports_direct_stream: true,
        ..Default::default()
    }
}

#[async_trait]
impl MediaSourceManager for StreamSources {
    async fn get_media_streams(&self, _item_id: Uuid) -> Result<Vec<MediaStream>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_media_attachments(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<MediaAttachment>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_playback_media_sources(
        &self,
        _item_id: Uuid,
        _user_id: Uuid,
        _allow_media_probe: bool,
        _enable_path_substitution: bool,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        Ok(vec![static_source(&self.path)])
    }
    async fn get_static_media_sources(
        &self,
        _item_id: Uuid,
        _enable_path_substitution: bool,
        _user_id: Option<Uuid>,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        Ok(vec![static_source(&self.path)])
    }
    async fn open_live_stream(
        &self,
        request: &LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        let live_id = "live-123".to_owned();
        self.opened
            .lock()
            .unwrap()
            .push(request.item_id.to_string());
        let mut source = static_source(&self.path);
        source.live_stream_id = Some(live_id);
        source.requires_closing = true;
        Ok(source)
    }
    async fn get_live_stream(&self, _id: &str) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!()
    }
    async fn refresh_media_streams(&self, _item_id: uuid::Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn close_live_stream(&self, id: &str) -> Result<(), ServiceError> {
        self.closed.lock().unwrap().push(id.to_owned());
        Ok(())
    }
}

/// A [`LibraryManager`] that resolves [`ITEM_ID`].
struct StreamLibrary;

#[async_trait]
impl LibraryManager for StreamLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((id == ITEM_ID).then(|| minimal_base_item(ITEM_ID, "A Movie", "Movie")))
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
        _c: hermit_model::data::CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn create_items(
        &self,
        _i: &[BaseItemEntity],
        _p: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_items(
        &self,
        _i: &[BaseItemEntity],
        _p: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn delete_item(&self, _id: Uuid, _o: &DeleteOptions) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_people(
        &self,
        _q: &InternalPeopleQuery,
    ) -> Result<Vec<hermit_db::entities::base_items::PeopleEntity>, ServiceError> {
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
    ) -> Result<hermit_model::dto::ItemCounts, ServiceError> {
        unimplemented!()
    }
    async fn get_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_query_filters_legacy(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<hermit_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!()
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: hermit_model::entities::MediaStreamType,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// The wired [`AppState`] plus the call-recording handles the tests assert on.
struct Harness {
    /// The assembled application state to build a router from.
    app: AppState,
    /// Item ids passed to `open_live_stream`.
    opened: Arc<Mutex<Vec<String>>>,
    /// Ids passed to `close_live_stream`.
    closed: Arc<Mutex<Vec<String>>>,
}

/// Builds a [`Harness`] serving `path` for streams.
fn state(path: &str) -> Harness {
    let opened = Arc::new(Mutex::new(Vec::new()));
    let closed = Arc::new(Mutex::new(Vec::new()));
    let sources = StreamSources {
        path: path.to_owned(),
        opened: opened.clone(),
        closed: closed.clone(),
    };
    let app = AppState::new(
        Arc::new(StreamLibrary),
        Arc::new(OkUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(sources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(hermit_api::test_support::FakeDto),
        Arc::new(OkAuth),
        Arc::new(OkAuth),
        Arc::new(FakeQuickConnect),
        Arc::new(FakePlaylists),
        Arc::new(FakeCollections),
        Arc::new(FakeTvSeries),
        Arc::new(hermit_api::test_support::FakeSubtitles),
        Arc::new(hermit_api::test_support::FakeLyrics),
        Arc::new(hermit_api::test_support::FakeMediaSegments),
        Arc::new(hermit_api::test_support::FakeTrickplay),
        Arc::new(hermit_api::test_support::FakeDevices),
        Arc::new(hermit_api::test_support::FakeClientEventLogger),
        Arc::new(hermit_api::test_support::FakeApiKeys),
        Arc::new(hermit_api::test_support::FakeLocalization),
        Arc::new(hermit_api::test_support::FakeDisplayPreferences),
        Arc::new(hermit_api::test_support::FakeActivity),
        Arc::new(hermit_api::test_support::FakeFileSystem),
        Arc::new(hermit_api::test_support::FakeTasks),
    );
    Harness {
        app,
        opened,
        closed,
    }
}

/// An RAII temp media file: writes known contents under the system temp dir and
/// removes the file on drop. Its `path` is served by the stub media source.
struct TempMedia {
    path: String,
}

impl TempMedia {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        // A unique name per invocation so parallel tests don't collide.
        path.push(format!("hermit-streaming-{}-movie.mkv", Uuid::new_v4()));
        std::fs::write(&path, b"hello-hermit-media").expect("write temp media");
        Self {
            path: path.to_string_lossy().into_owned(),
        }
    }
}

impl Drop for TempMedia {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Writes a temp file with known contents and returns its RAII guard + path.
fn temp_media() -> (TempMedia, String) {
    let media = TempMedia::new();
    let path = media.path.clone();
    (media, path)
}

fn authed(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", "Bearer token")
        .body(Body::empty())
        .expect("request")
}

#[tokio::test]
async fn download_sets_content_disposition() {
    let (_dir, path) = temp_media();
    let app = state(&path).app;
    let router = create_router(app);
    let resp = router
        .oneshot(authed("GET", &format!("/Items/{ITEM_ID}/Download")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let cd = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(cd.contains("attachment"), "got: {cd}");
    assert!(cd.contains("movie.mkv"), "got: {cd}");
}

#[tokio::test]
async fn bitrate_test_returns_requested_size() {
    let (_dir, path) = temp_media();
    let app = state(&path).app;
    let router = create_router(app);
    let resp = router
        .oneshot(authed("GET", "/Playback/BitrateTest?size=2048"))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.len(), 2048);
}

#[tokio::test]
async fn bitrate_test_rejects_oversize() {
    let (_dir, path) = temp_media();
    let app = state(&path).app;
    let router = create_router(app);
    let resp = router
        .oneshot(authed("GET", "/Playback/BitrateTest?size=999999999"))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn live_stream_open_then_close() {
    let (_dir, path) = temp_media();
    let h = state(&path);
    let (opened, closed) = (h.opened.clone(), h.closed.clone());
    let router = create_router(h.app);

    let open = router
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/LiveStreams/Open?itemId={ITEM_ID}"),
        ))
        .await
        .expect("open response");
    assert_eq!(open.status(), StatusCode::OK);
    assert_eq!(opened.lock().unwrap().len(), 1);

    let close = router
        .oneshot(authed("POST", "/LiveStreams/Close?liveStreamId=live-123"))
        .await
        .expect("close response");
    assert_eq!(close.status(), StatusCode::NO_CONTENT);
    assert_eq!(closed.lock().unwrap().as_slice(), ["live-123"]);
}
