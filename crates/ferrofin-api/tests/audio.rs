//! Audio handler tests: direct/container + universal audio streaming, and Lyrics
//! read/delete/remote-search.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate and return canned data. The
//! direct stream routes serve a real temp file so the `ServeFile` path is
//! exercised end to end.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeAppHost, FakeCollections, FakeConfig, FakeMediaSegments, FakeMusic, FakePlaylists,
    FakeProviders, FakeQuickConnect, FakeSearch, FakeSessions, FakeSimilarItems, FakeSubtitles,
    FakeSystem, FakeTrickplay, FakeTvSeries, FakeUserData, FakeUserViews, minimal_base_item,
};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::{MediaSourceInfo, MediaSourceType};
use ferrofin_model::entities_media::{MediaAttachment, MediaStream};
use ferrofin_model::lyrics::{LyricDto, RemoteLyricInfoDto};
use ferrofin_model::media_info::{LiveStreamRequest, MediaProtocol};
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, MediaSourceManager, UserManager};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{
    AuthorizationInfo, DeleteOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::persistence::ItemWithCounts;
use ferrofin_traits::stubs::LyricManager;
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
    ) -> Result<Vec<ferrofin_model::dto::NameIdPair>, ServiceError> {
        unimplemented!()
    }
    async fn get_password_reset_providers(
        &self,
    ) -> Result<Vec<ferrofin_model::dto::NameIdPair>, ServiceError> {
        unimplemented!()
    }
    async fn get_user_dto(
        &self,
        _user: &UserEntity,
        _server_id: Option<String>,
    ) -> Result<ferrofin_model::dto::UserDto, ServiceError> {
        unimplemented!()
    }
    async fn update_configuration(
        &self,
        _user_id: Uuid,
        _config: &ferrofin_model::configuration::UserConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_policy(
        &self,
        _user_id: Uuid,
        _policy: &ferrofin_model::users::UserPolicy,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn clear_profile_image(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`MediaSourceManager`] that serves a static source for a temp file.
struct StreamSources {
    /// The on-disk path the static source points at.
    path: String,
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
        _request: &LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!()
    }
    async fn get_live_stream(&self, _id: &str) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!()
    }
    async fn refresh_media_streams(&self, _item_id: uuid::Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`LibraryManager`] that resolves [`ITEM_ID`] and 404s everything else.
struct StreamLibrary;

#[async_trait]
impl LibraryManager for StreamLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((id == ITEM_ID).then(|| minimal_base_item(ITEM_ID, "A Song", "Audio")))
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
        _c: ferrofin_model::data::CollectionType,
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
    ) -> Result<Vec<ferrofin_db::entities::base_items::PeopleEntity>, ServiceError> {
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
    ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!()
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: ferrofin_model::entities::MediaStreamType,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`LyricManager`] whose stored lyrics / mutation results are configurable.
struct CannedLyrics {
    stored: Option<LyricDto>,
    deleted: Arc<Mutex<bool>>,
}

#[async_trait]
impl LyricManager for CannedLyrics {
    async fn get_lyrics(&self, _item_id: Uuid) -> Result<Option<LyricDto>, ServiceError> {
        Ok(self.stored.clone())
    }
    async fn search_lyrics(&self, _item_id: Uuid) -> Result<Vec<RemoteLyricInfoDto>, ServiceError> {
        Ok(Vec::new())
    }
    async fn download_lyrics(
        &self,
        _item_id: Uuid,
        _lyric_id: &str,
    ) -> Result<Option<LyricDto>, ServiceError> {
        Ok(None)
    }
    async fn save_lyric(
        &self,
        _item_id: Uuid,
        _format: &str,
        _lyrics: &str,
    ) -> Result<Option<LyricDto>, ServiceError> {
        Ok(None)
    }
    async fn delete_lyrics(&self, _item_id: Uuid) -> Result<(), ServiceError> {
        *self.deleted.lock().unwrap() = true;
        Ok(())
    }
    async fn get_supported_providers(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::LyricProviderInfo>, ServiceError> {
        Ok(Vec::new())
    }
}

/// Builds an [`AppState`] serving `path` for streams and using `lyrics` for the
/// Lyrics routes; every other manager is a shared panic fake.
fn state(path: &str, lyrics: Arc<dyn LyricManager>) -> AppState {
    AppState::new(
        Arc::new(StreamLibrary),
        Arc::new(OkUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(StreamSources {
            path: path.to_owned(),
        }),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(ferrofin_api::test_support::FakeDto),
        Arc::new(OkAuth),
        Arc::new(OkAuth),
        Arc::new(FakeQuickConnect),
        Arc::new(FakePlaylists),
        Arc::new(FakeCollections),
        Arc::new(FakeTvSeries),
        Arc::new(FakeSubtitles),
        lyrics,
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
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

/// A [`LyricManager`] that panics if reached — for the stream tests that never
/// touch lyrics.
fn no_lyrics() -> Arc<dyn LyricManager> {
    Arc::new(ferrofin_api::test_support::FakeLyrics)
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
        path.push(format!("ferrofin-audio-{}-movie.mkv", Uuid::new_v4()));
        std::fs::write(&path, b"hello-ferrofin-media").expect("write temp media");
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

/// Sends an authenticated request and returns `(status, body-bytes)`.
async fn call(app: AppState, method: &str, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = create_router(app)
        .oneshot(authed(method, uri))
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
async fn audio_stream_by_container_serves_file() {
    let (_dir, path) = temp_media();
    let app = state(&path, no_lyrics());
    let router = create_router(app);
    // `/Audio/{itemId}/stream.mp3` normalizes to `/Audio/{itemId}/{container}`.
    let resp = router
        .oneshot(authed("GET", &format!("/Audio/{ITEM_ID}/stream.mp3")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn universal_audio_serves_file() {
    let (_dir, path) = temp_media();
    let app = state(&path, no_lyrics());
    let router = create_router(app);
    let resp = router
        .oneshot(authed("GET", &format!("/Audio/{ITEM_ID}/universal")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn lyrics_get_returns_stored_or_404() {
    let (_dir, path) = temp_media();
    let mut dto = LyricDto::default();
    dto.lyrics.push(ferrofin_model::lyrics::LyricLine {
        text: "la la la".to_owned(),
        start: Some(0),
        cues: None,
    });
    let app = state(
        &path,
        Arc::new(CannedLyrics {
            stored: Some(dto),
            deleted: Arc::new(Mutex::new(false)),
        }),
    );
    let (ok, body) = call(app, "GET", &format!("/Audio/{ITEM_ID}/Lyrics")).await;
    assert_eq!(ok, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Lyrics"][0]["Text"], "la la la");

    // No stored lyrics → 404.
    let (_dir, path) = temp_media();
    let app = state(
        &path,
        Arc::new(CannedLyrics {
            stored: None,
            deleted: Arc::new(Mutex::new(false)),
        }),
    );
    let (missing, _) = call(app, "GET", &format!("/Audio/{ITEM_ID}/Lyrics")).await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn lyrics_delete_is_204_and_calls_manager() {
    let (_dir, path) = temp_media();
    let deleted = Arc::new(Mutex::new(false));
    let app = state(
        &path,
        Arc::new(CannedLyrics {
            stored: None,
            deleted: deleted.clone(),
        }),
    );
    let (status, _) = call(app, "DELETE", &format!("/Audio/{ITEM_ID}/Lyrics")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(*deleted.lock().unwrap());
}

#[tokio::test]
async fn lyrics_remote_search_is_empty_list() {
    let (_dir, path) = temp_media();
    let app = state(
        &path,
        Arc::new(CannedLyrics {
            stored: None,
            deleted: Arc::new(Mutex::new(false)),
        }),
    );
    let (status, body) = call(app, "GET", &format!("/Audio/{ITEM_ID}/RemoteSearch/Lyrics")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.as_array().unwrap().is_empty());
}
