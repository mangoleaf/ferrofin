//! Video handler tests: direct stream (full body + Range), version-group
//! management (merge/split/alternate-sources), additional parts, and subtitle
//! upload/delete.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate and return canned data. The
//! direct stream / download routes serve a real temp file so the `ServeFile`
//! Range path is exercised end to end.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeAppHost, FakeCollections, FakeConfig, FakeMusic, FakePlaylists, FakeProviders,
    FakeQuickConnect, FakeSearch, FakeSessions, FakeSimilarItems, FakeSubtitles, FakeSystem,
    FakeTvSeries, FakeUserData, FakeUserViews, minimal_base_item,
};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::{MediaSourceInfo, MediaSourceType};
use ferrofin_model::entities::VideoType;
use ferrofin_model::entities_media::{MediaAttachment, MediaStream};
use ferrofin_model::media_info::{LiveStreamRequest, MediaProtocol};
use ferrofin_model::providers::RemoteSubtitleInfo;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, MediaSourceManager, UserManager};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{
    AuthorizationInfo, DeleteOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::persistence::ItemWithCounts;
use ferrofin_traits::subtitles::{SubtitleManager, SubtitleResponse, SubtitleSearchRequest};
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x00A1_0000);
const ITEM_ID: Uuid = Uuid::from_u128(0x00A1_0001);
/// A stacked movie: the first part of three.
const STACKED_ID: Uuid = Uuid::from_u128(0x00A1_0002);
/// [`STACKED_ID`]'s other parts.
const PART_IDS: [Uuid; 2] = [Uuid::from_u128(0x00A1_0003), Uuid::from_u128(0x00A1_0004)];

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
        normalized_username: "ALICE".to_owned(),
    }
}

/// An [`AuthService`]/[`AuthorizationContext`] that authenticates as [`USER_ID`].
/// `elevated` authenticates as an API key, satisfying `RequiresElevation` on
/// the admin-only video routes (`MergeVersions`, `AlternateSources`, subtitle
/// delete). Off by default so the playback routes keep proving they work for an
/// ordinary user — over-gating those would break streaming for everyone.
struct OkAuth {
    elevated: bool,
}

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(user()),
            is_api_key: self.elevated,
            is_authenticated: true,
            user_policy: Some(Arc::new(ferrofin_model::users::UserPolicy {
                enable_remote_access: true,
                enable_subtitle_management: true,
                ..Default::default()
            })),
            ..ferrofin_api::test_support::authenticated_user_info()
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
            is_api_key: self.elevated,
            is_authenticated: true,
            user_policy: Some(Arc::new(ferrofin_model::users::UserPolicy {
                enable_remote_access: true,
                enable_subtitle_management: true,
                ..Default::default()
            })),
            ..ferrofin_api::test_support::authenticated_user_info()
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
#[derive(Default)]
struct StreamSources {
    /// The on-disk path the static source points at.
    path: String,
    /// A second version's path, listed under [`ALT_ID`] after the item's own.
    alternate: Option<String>,
    /// The path of the live stream open under [`LIVE_STREAM_ID`], if any.
    live: Option<String>,
    /// Whether the item's own source is a Blu-ray folder rip.
    disc: bool,
    /// The HTTP URL the item's own source is read from instead of its path
    /// (an in-progress recording's `EncoderPath`), if any.
    encoder: Option<String>,
}

impl StreamSources {
    /// A single-version item read from `path`, with no live stream open.
    fn at(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            ..Self::default()
        }
    }
}

/// The id of the item's second version, when [`StreamSources`] has one.
const ALT_ID: Uuid = Uuid::from_u128(0x00A1_00A1);

/// The `User-Agent` the open live stream's tuner requires.
const TUNER_AGENT: &str = "TunerAgent/1.0";

/// The id the open live stream of [`StreamSources`] is known by.
const LIVE_STREAM_ID: &str = "prov_service_live-1";

fn static_source(path: &str) -> MediaSourceInfo {
    MediaSourceInfo {
        id: Some(ITEM_ID.to_string()),
        path: Some(path.to_owned()),
        // A remote target (a `.strm` shortcut's) is read over its own protocol.
        protocol: if path.starts_with("http") {
            MediaProtocol::Http
        } else if path.starts_with("rtsp") {
            MediaProtocol::Rtsp
        } else {
            MediaProtocol::File
        },
        is_remote: !path.starts_with('/'),
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
        let mut sources = vec![MediaSourceInfo {
            video_type: self.disc.then_some(VideoType::BluRay),
            encoder_protocol: self.encoder.as_ref().map(|_| MediaProtocol::Http),
            encoder_path: self.encoder.clone(),
            ..static_source(&self.path)
        }];
        if let Some(alternate) = &self.alternate {
            sources.push(MediaSourceInfo {
                id: Some(ALT_ID.to_string()),
                ..static_source(alternate)
            });
        }
        Ok(sources)
    }
    async fn open_live_stream(
        &self,
        _request: &LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!()
    }
    async fn get_live_stream(&self, id: &str) -> Result<MediaSourceInfo, ServiceError> {
        match &self.live {
            Some(path) if id == LIVE_STREAM_ID => Ok(MediaSourceInfo {
                live_stream_id: Some(LIVE_STREAM_ID.to_owned()),
                required_http_headers: [("User-Agent".to_owned(), TUNER_AGENT.to_owned())].into(),
                ..static_source(path)
            }),
            _ => Err(ServiceError::NotFound(format!("live stream {id}"))),
        }
    }
    async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`LibraryManager`] that resolves [`ITEM_ID`] and records merge/remove calls.
struct StreamLibrary {
    merged: Arc<Mutex<Vec<Vec<Uuid>>>>,
    removed: Arc<Mutex<Vec<Uuid>>>,
}

/// A [`MergeVersionsManager`] recording which bulk op each route invoked.
struct FakeMergeVersions {
    /// Bulk `MergeVersions`-plugin ops invoked, in order.
    bulk: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl ferrofin_traits::merge_versions::MergeVersionsManager for FakeMergeVersions {
    async fn merge_movies(
        &self,
        _progress: Option<ferrofin_traits::merge_versions::MergeProgress<'_>>,
    ) -> Result<(), ServiceError> {
        self.bulk.lock().unwrap().push("merge_movies");
        Ok(())
    }
    async fn split_movies(
        &self,
        _progress: Option<ferrofin_traits::merge_versions::MergeProgress<'_>>,
    ) -> Result<(), ServiceError> {
        self.bulk.lock().unwrap().push("split_movies");
        Ok(())
    }
    async fn merge_episodes(
        &self,
        _progress: Option<ferrofin_traits::merge_versions::MergeProgress<'_>>,
    ) -> Result<(), ServiceError> {
        self.bulk.lock().unwrap().push("merge_episodes");
        Ok(())
    }
    async fn split_episodes(
        &self,
        _progress: Option<ferrofin_traits::merge_versions::MergeProgress<'_>>,
    ) -> Result<(), ServiceError> {
        self.bulk.lock().unwrap().push("split_episodes");
        Ok(())
    }
}

#[async_trait]
impl LibraryManager for StreamLibrary {
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
        Ok(match id {
            ITEM_ID => Some(minimal_base_item(ITEM_ID, "A Movie", "Movie")),
            STACKED_ID => Some(minimal_base_item(STACKED_ID, "Heist", "Movie")),
            _ => None,
        })
    }
    async fn merge_versions(&self, ids: &[Uuid]) -> Result<(), ServiceError> {
        self.merged.lock().unwrap().push(ids.to_vec());
        Ok(())
    }
    async fn remove_alternate_sources(&self, item_id: Uuid) -> Result<(), ServiceError> {
        self.removed.lock().unwrap().push(item_id);
        Ok(())
    }
    async fn get_additional_parts(
        &self,
        video: &BaseItemEntity,
        user: Option<&UserEntity>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // ITEM_ID is a lone movie; STACKED_ID a stack of three parts, asked
        // for as the requesting user.
        assert_eq!(user.map(|u| u.username.as_str()), Some("alice"));
        if video.id == STACKED_ID.to_string() {
            Ok(vec![
                minimal_base_item(PART_IDS[0], "Heist cd2", "Video"),
                minimal_base_item(PART_IDS[1], "Heist cd3", "Video"),
            ])
        } else {
            Ok(Vec::new())
        }
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

/// A [`SubtitleManager`] recording deletes and returning empty search results.
struct CannedSubtitles {
    deleted: Arc<Mutex<Vec<(Uuid, i32)>>>,
}

#[async_trait]
impl SubtitleManager for CannedSubtitles {
    async fn search_subtitles(
        &self,
        _request: &SubtitleSearchRequest,
    ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn download_subtitles(
        &self,
        _item_id: Uuid,
        _subtitle_id: &str,
    ) -> Result<(), ServiceError> {
        Err(ServiceError::invalid_input("no providers"))
    }
    async fn upload_subtitle(
        &self,
        _item_id: Uuid,
        _response: &SubtitleResponse,
    ) -> Result<(), ServiceError> {
        Err(ServiceError::invalid_input("no providers"))
    }
    async fn get_remote_subtitles(&self, _id: &str) -> Result<SubtitleResponse, ServiceError> {
        Err(ServiceError::invalid_input("no providers"))
    }
    async fn delete_subtitles(&self, item_id: Uuid, index: i32) -> Result<(), ServiceError> {
        self.deleted.lock().unwrap().push((item_id, index));
        Ok(())
    }
    async fn get_supported_providers(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::SubtitleProviderInfo>, ServiceError> {
        Ok(Vec::new())
    }
}

/// The wired [`AppState`] plus the call-recording handles the tests assert on.
struct Harness {
    /// The assembled application state to build a router from.
    app: AppState,
    /// Id lists passed to `merge_versions`.
    merged: Arc<Mutex<Vec<Vec<Uuid>>>>,
    /// Ids passed to `remove_alternate_sources`.
    removed: Arc<Mutex<Vec<Uuid>>>,
    /// Bulk `MergeVersions`-plugin ops invoked, in order.
    bulk: Arc<Mutex<Vec<&'static str>>>,
}

/// The ordinary-user harness.
fn state(path: &str, subtitles: Arc<dyn SubtitleManager>) -> Harness {
    state_with(
        StreamSources::at(path),
        subtitles,
        false,
        Arc::new(ferrofin_api::test_support::FakeDto),
    )
}

/// The elevated harness, for the video routes that are `RequiresElevation`
/// upstream: `POST /Videos/MergeVersions`,
/// `DELETE /Videos/{itemId}/AlternateSources`, and
/// `DELETE /Videos/{itemId}/Subtitles/{index}`.
fn elevated_state(path: &str, subtitles: Arc<dyn SubtitleManager>) -> Harness {
    state_with(
        StreamSources::at(path),
        subtitles,
        true,
        Arc::new(ferrofin_api::test_support::FakeDto),
    )
}

/// Builds a [`Harness`] serving `sources` for streams and using `subtitles` for
/// the subtitle routes.
fn state_with(
    sources: StreamSources,
    subtitles: Arc<dyn SubtitleManager>,
    elevated: bool,
    dto: Arc<dyn ferrofin_traits::dto::DtoService>,
) -> Harness {
    let merged = Arc::new(Mutex::new(Vec::new()));
    let removed = Arc::new(Mutex::new(Vec::new()));
    let bulk = Arc::new(Mutex::new(Vec::new()));
    let library = StreamLibrary {
        merged: merged.clone(),
        removed: removed.clone(),
    };
    let app = AppState::new(
        Arc::new(library),
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
        dto,
        Arc::new(OkAuth { elevated }),
        Arc::new(OkAuth { elevated }),
        Arc::new(FakeQuickConnect),
        Arc::new(FakePlaylists),
        Arc::new(FakeCollections),
        Arc::new(FakeTvSeries),
        subtitles,
        Arc::new(ferrofin_api::test_support::FakeLyrics),
        Arc::new(ferrofin_api::test_support::FakeMediaSegments),
        Arc::new(ferrofin_api::test_support::FakeTrickplay),
        Arc::new(ferrofin_api::test_support::FakeDevices),
        Arc::new(ferrofin_api::test_support::FakeClientEventLogger),
        Arc::new(ferrofin_api::test_support::FakeApiKeys),
        Arc::new(ferrofin_api::test_support::FakeLocalization),
        Arc::new(ferrofin_api::test_support::FakeDisplayPreferences),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(ferrofin_api::test_support::FakeFileSystem),
        Arc::new(ferrofin_api::test_support::FakeTasks),
    )
    .with_merge_versions(Arc::new(FakeMergeVersions { bulk: bulk.clone() }));
    Harness {
        app,
        merged,
        removed,
        bulk,
    }
}

/// A [`SubtitleManager`] that panics if reached — for tests that never touch it.
fn no_subtitles() -> Arc<dyn SubtitleManager> {
    Arc::new(FakeSubtitles)
}

/// An RAII temp media file: writes known contents under the system temp dir and
/// removes the file on drop. Its `path` is served by the stub media source.
struct TempMedia {
    path: String,
}

impl TempMedia {
    fn new(contents: &[u8]) -> Self {
        let mut path = std::env::temp_dir();
        // A unique name per invocation so parallel tests don't collide.
        path.push(format!("ferrofin-videos-{}-movie.mkv", Uuid::new_v4()));
        std::fs::write(&path, contents).expect("write temp media");
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

/// Writes a temp file with the default contents and returns its RAII guard + path.
fn temp_media() -> (TempMedia, String) {
    let media = TempMedia::new(b"hello-ferrofin-media");
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
        .header("Authorization", "Bearer token");
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

// ---- Direct stream ---------------------------------------------------------

#[tokio::test]
async fn video_stream_serves_file() {
    let (_dir, path) = temp_media();
    let app = state(&path, no_subtitles()).app;
    let router = create_router(app);
    let resp = router
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/stream?static=true"),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"hello-ferrofin-media");
}

#[tokio::test]
async fn video_stream_serves_full_body() {
    let media = TempMedia::new(b"0123456789");
    let path = media.path.clone();
    let router = create_router(state(&path, no_subtitles()).app);
    let response = router
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/stream?static=true"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"0123456789");
}

#[tokio::test]
async fn video_stream_range_request_is_206_with_content_range() {
    let media = TempMedia::new(b"0123456789");
    let path = media.path.clone();
    let router = create_router(state(&path, no_subtitles()).app);
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Videos/{ITEM_ID}/stream?static=true"))
                .header("Authorization", "Bearer token")
                .header(header::RANGE, "bytes=2-5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let content_range = response
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(content_range, "bytes 2-5/10");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"2345");
}

#[tokio::test]
async fn video_stream_missing_file_is_404() {
    // The media source resolves (the DB has a path) but the file is gone —
    // a stale NFS handle, a moved file, an unmounted share. That must be a 404,
    // and the fs-level cause is logged on that branch (the diagnostic used to
    // run as an unconditional pre-`stat` on every segment/Range request).
    let mut path = std::env::temp_dir();
    path.push(format!("ferrofin-absent-{}-movie.mkv", Uuid::new_v4()));
    let path = path.to_string_lossy().into_owned();
    let router = create_router(state(&path, no_subtitles()).app);
    let response = router
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/stream?static=true"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Serves `contents` over HTTP on a loopback port, with `Range` support (the
/// remote origin a `.strm` shortcut points at), and returns its URL.
async fn remote_origin(contents: &'static [u8]) -> String {
    use tower_http::services::ServeFile;
    let dir = std::env::temp_dir().join(format!("ferrofin-origin-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("movie.mkv");
    std::fs::write(&file, contents).unwrap();
    let app = axum::Router::new().route_service("/movie.mkv", ServeFile::new(file));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}/movie.mkv")
}

/// `GetStaticRemoteStreamResult`: a static stream of a source the item takes
/// from a remote HTTP target is relayed from it — whole, and with the client's
/// `Range` forwarded so the answer is the upstream's `206` and `Content-Range`.
#[tokio::test]
async fn a_static_remote_stream_is_relayed_with_its_range() {
    let url = remote_origin(b"0123456789").await;
    let router = create_router(state(&url, no_subtitles()).app);
    let whole = router
        .clone()
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/stream?static=true"),
        ))
        .await
        .unwrap();
    assert_eq!(whole.status(), StatusCode::OK);
    assert_eq!(whole.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(whole.headers()[header::CONTENT_LENGTH], "10");
    let bytes = axum::body::to_bytes(whole.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"0123456789");

    let part = router
        .oneshot(
            Request::builder()
                .uri(format!("/Videos/{ITEM_ID}/stream?static=true"))
                .header("Authorization", "Bearer token")
                .header(header::RANGE, "bytes=2-5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    let bytes = axum::body::to_bytes(part.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"2345");
}

/// Only an HTTP target is relayed: any other non-file protocol is
/// `400 Input protocol … cannot be streamed statically`, and an origin that
/// cannot be reached is a server error, not a file `404`.
#[tokio::test]
async fn a_static_stream_of_another_protocol_is_refused_and_a_dead_origin_fails() {
    let rtsp = create_router(state("rtsp://camera/feed", no_subtitles()).app)
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/stream?static=true"),
        ))
        .await
        .unwrap();
    assert_eq!(rtsp.status(), StatusCode::BAD_REQUEST);

    // Nothing listens on this port any more.
    let dead = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/movie.mkv", listener.local_addr().unwrap())
    };
    let down = create_router(state(&dead, no_subtitles()).app)
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/stream?static=true"),
        ))
        .await
        .unwrap();
    assert_eq!(down.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

/// The status and body `GET uri` answers with.
async fn get_body(router: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = router.clone().oneshot(authed("GET", uri)).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

/// `StreamingHelpers.GetStreamingState`: a direct-play request plays the
/// version its `mediaSourceId` names (jellyfin-web puts it on every
/// `stream.{container}?Static=true` URL), in either GUID spelling. The item's
/// own id, or none, is its first version; an id naming no version is `404`,
/// never the wrong file. Video, audio and universal audio all select alike.
#[tokio::test]
async fn a_static_stream_plays_the_version_its_media_source_id_names() {
    let primary = TempMedia::new(b"primary");
    let alternate = TempMedia::new(b"alternate");
    let sources = StreamSources {
        alternate: Some(alternate.path.clone()),
        ..StreamSources::at(&primary.path)
    };
    let router = create_router(
        state_with(
            sources,
            no_subtitles(),
            false,
            Arc::new(ferrofin_api::test_support::FakeDto),
        )
        .app,
    );
    let unknown = Uuid::from_u128(0xDEAD);
    for route in [
        format!("/Videos/{ITEM_ID}/stream?static=true"),
        format!("/Videos/{ITEM_ID}/stream.mkv?Static=true"),
        format!("/Audio/{ITEM_ID}/stream.mp3?static=true"),
        format!("/Audio/{ITEM_ID}/universal?deviceId=d1"),
    ] {
        for (media_source_id, expected) in [
            (ALT_ID.simple().to_string(), Some(&b"alternate"[..])),
            (ALT_ID.to_string(), Some(&b"alternate"[..])),
            (ITEM_ID.simple().to_string(), Some(&b"primary"[..])),
            (String::new(), Some(&b"primary"[..])),
            (unknown.simple().to_string(), None),
        ] {
            let uri = format!("{route}&mediaSourceId={media_source_id}");
            let (status, body) = get_body(&router, &uri).await;
            match expected {
                Some(expected) => {
                    assert_eq!(status, StatusCode::OK, "{uri}");
                    assert_eq!(body, expected, "{uri}");
                }
                None => assert_eq!(status, StatusCode::NOT_FOUND, "{uri}"),
            }
        }
    }
}

/// An origin on a loopback port that answers with the `User-Agent` it was sent
/// (empty when none), and returns its URL: what a relay sent, and that it was
/// this origin it read.
async fn agent_origin() -> String {
    let app = axum::Router::new().route(
        "/live.ts",
        axum::routing::get(|headers: axum::http::HeaderMap| async move {
            headers
                .get(header::USER_AGENT)
                .map(|agent| agent.as_bytes().to_vec())
                .unwrap_or_default()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}/live.ts")
}

/// The `User-Agent` the relay names the server with when a source requires
/// none (`NamedClient.Default`).
const SERVER_AGENT: &str = concat!("Ferrofin/", env!("CARGO_PKG_VERSION"));

/// `GetStaticRemoteStreamResult`: a remote target that requires no
/// `User-Agent` is relayed under the server's own, never with none at all
/// (origins commonly refuse an agentless request).
#[tokio::test]
async fn a_static_remote_stream_names_the_server_when_no_agent_is_required() {
    let url = agent_origin().await;
    let (status, body) = get_body(
        &create_router(state(&url, no_subtitles()).app),
        &format!("/Videos/{ITEM_ID}/stream?static=true"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, SERVER_AGENT.as_bytes());
}

/// `EncodingHelper.AttachMediaSourceInfo`: a source naming an encoder path (an
/// in-progress recording's `LiveRecordings` URL, which follows the file as it
/// grows) is read from it, not from the partial file on disk.
#[tokio::test]
async fn a_static_stream_is_read_from_the_encoder_path_its_source_names() {
    let partial = TempMedia::new(b"recorded so far");
    let sources = StreamSources {
        encoder: Some(agent_origin().await),
        ..StreamSources::at(&partial.path)
    };
    let router = create_router(
        state_with(
            sources,
            no_subtitles(),
            false,
            Arc::new(ferrofin_api::test_support::FakeDto),
        )
        .app,
    );
    let (status, body) = get_body(&router, &format!("/Videos/{ITEM_ID}/stream?static=true")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, SERVER_AGENT.as_bytes());
}

/// `StreamingHelpers.GetStreamingState`: a `LiveStreamId` wins over the
/// item's static sources, so the stream already open is what plays rather than
/// a second dial of the channel. A pass-through stream keeps its tuner URL and
/// is relayed with the tuner's required `User-Agent`
/// (`GetStaticRemoteStreamResult`); a stream that is no longer open is `404`.
#[tokio::test]
async fn a_static_stream_with_a_live_stream_id_plays_the_open_stream() {
    let channel = TempMedia::new(b"unopened channel");
    let tuner = agent_origin().await;
    let sources = StreamSources {
        live: Some(tuner),
        ..StreamSources::at(&channel.path)
    };
    let router = create_router(
        state_with(
            sources,
            no_subtitles(),
            false,
            Arc::new(ferrofin_api::test_support::FakeDto),
        )
        .app,
    );
    let (status, body) = get_body(
        &router,
        &format!("/Videos/{ITEM_ID}/stream.ts?static=true&LiveStreamId={LIVE_STREAM_ID}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The tuner's own agent came back: the open stream was read, with it.
    assert_eq!(body, TUNER_AGENT.as_bytes());
    let (status, _) = get_body(
        &router,
        &format!("/Videos/{ITEM_ID}/stream.ts?static=true&LiveStreamId=prov_service_closed"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// `VideosController.GetVideoStream`: a Blu-ray or DVD folder rip is never
/// served as a static file; the request goes to ffmpeg, which is unavailable
/// here (`404`). Audio has no such exclusion.
#[tokio::test]
async fn a_static_video_stream_of_a_disc_rip_is_transcoded() {
    let rip = TempMedia::new(b"disc");
    let sources = StreamSources {
        disc: true,
        ..StreamSources::at(&rip.path)
    };
    let router = create_router(
        state_with(
            sources,
            no_subtitles(),
            false,
            Arc::new(ferrofin_api::test_support::FakeDto),
        )
        .app,
    );
    let (video, _) = get_body(&router, &format!("/Videos/{ITEM_ID}/stream?static=true")).await;
    assert_eq!(video, StatusCode::NOT_FOUND);
    let (audio, body) = get_body(&router, &format!("/Audio/{ITEM_ID}/stream?static=true")).await;
    assert_eq!(audio, StatusCode::OK);
    assert_eq!(body, b"disc");
}

// ---- Version-group management ----------------------------------------------

#[tokio::test]
async fn merge_versions_requires_two_ids() {
    let (_dir, path) = temp_media();
    let h = elevated_state(&path, no_subtitles());
    let merged = h.merged.clone();
    let router = create_router(h.app);
    let resp = router
        .oneshot(authed(
            "POST",
            &format!("/Videos/MergeVersions?ids={ITEM_ID}"),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(merged.lock().unwrap().is_empty());
}

#[tokio::test]
async fn merge_versions_merges_two_ids() {
    let (_dir, path) = temp_media();
    let h = elevated_state(&path, no_subtitles());
    let merged = h.merged.clone();
    let router = create_router(h.app);
    let other = STACKED_ID;
    let resp = router
        .oneshot(authed(
            "POST",
            &format!("/Videos/MergeVersions?ids={ITEM_ID},{other}"),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(merged.lock().unwrap().len(), 1);
    assert_eq!(merged.lock().unwrap()[0].len(), 2);
}

#[tokio::test]
async fn delete_alternate_sources_ok() {
    let (_dir, path) = temp_media();
    let h = elevated_state(&path, no_subtitles());
    let removed = h.removed.clone();
    let router = create_router(h.app);
    let resp = router
        .oneshot(authed(
            "DELETE",
            &format!("/Videos/{ITEM_ID}/AlternateSources"),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(removed.lock().unwrap().as_slice(), [ITEM_ID]);
}

/// The four parameterless `MergeVersions`-plugin routes each drive their bulk
/// manager op and return `204`.
#[tokio::test]
async fn merge_versions_plugin_routes_drive_bulk_ops() {
    let cases = [
        ("/MergeVersions/MergeMovies", "merge_movies"),
        ("/MergeVersions/SplitMovies", "split_movies"),
        ("/MergeVersions/MergeEpisodes", "merge_episodes"),
        ("/MergeVersions/SplitEpisodes", "split_episodes"),
    ];
    for (route, expected) in cases {
        let (_dir, path) = temp_media();
        let h = state(&path, no_subtitles());
        let bulk = h.bulk.clone();
        let router = create_router(h.app);
        let resp = router
            .oneshot(authed("POST", route))
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "{route}");
        assert_eq!(bulk.lock().unwrap().as_slice(), [expected], "{route}");
    }
}

/// When the Merge Versions extension is not wired (`AppState::merge_versions`
/// unset), every route reports `404` — the `service()` resolver's absent-plugin
/// branch, matching a Jellyfin server with the plugin's controller unregistered.
#[tokio::test]
async fn merge_versions_routes_404_when_plugin_unavailable() {
    for route in [
        "/MergeVersions/MergeMovies",
        "/MergeVersions/SplitMovies",
        "/MergeVersions/MergeEpisodes",
        "/MergeVersions/SplitEpisodes",
    ] {
        // A fake state with no `with_merge_versions` — merge_versions stays None.
        let router = create_router(ferrofin_api::test_support::authed_fake_state());
        let resp = router
            .oneshot(authed("POST", route))
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{route}");
    }
}

// ---- Additional parts ------------------------------------------------------

#[tokio::test]
async fn additional_parts_empty_for_known_item() {
    let (_dir, path) = temp_media();
    let app = state(&path, no_subtitles()).app;
    let router = create_router(app);
    let resp = router
        .oneshot(authed("GET", &format!("/Videos/{ITEM_ID}/AdditionalParts")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["Items"].as_array().map(Vec::len), Some(0));
    assert_eq!(json["TotalRecordCount"].as_i64(), Some(0));
}

/// A [`DtoService`](ferrofin_traits::dto::DtoService) projecting each row to
/// its id and name, and recording the owner it was given.
struct PartsDto {
    owners: Arc<Mutex<Vec<Option<Uuid>>>>,
}

#[async_trait]
impl ferrofin_traits::dto::DtoService for PartsDto {
    async fn get_primary_image_aspect_ratio(
        &self,
        _item_id: Uuid,
    ) -> Result<Option<f64>, ServiceError> {
        unimplemented!()
    }
    async fn get_base_item_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &ferrofin_traits::options::DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
    ) -> Result<ferrofin_model::dto::BaseItemDto, ServiceError> {
        unimplemented!()
    }
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        _options: &ferrofin_traits::options::DtoOptions,
        _user: Option<&UserEntity>,
        owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<ferrofin_model::dto::BaseItemDto>, ServiceError> {
        self.owners.lock().unwrap().push(owner_id);
        Ok(items
            .iter()
            .map(|i| ferrofin_model::dto::BaseItemDto {
                id: Uuid::parse_str(&i.id).unwrap(),
                name: i.name.clone(),
                ..Default::default()
            })
            .collect())
    }
    async fn get_item_by_name_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &ferrofin_traits::options::DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<ferrofin_model::dto::BaseItemDto, ServiceError> {
        unimplemented!()
    }
}

/// Port of `VideosController.GetAdditionalPart`: a stacked video's other
/// parts, in the order the library returns them, projected with the video as
/// their owner, counted as a `QueryResult`.
#[tokio::test]
async fn additional_parts_of_a_stack_are_its_other_parts() {
    let (_dir, path) = temp_media();
    let owners = Arc::new(Mutex::new(Vec::new()));
    let app = state_with(
        StreamSources::at(&path),
        no_subtitles(),
        false,
        Arc::new(PartsDto {
            owners: owners.clone(),
        }),
    )
    .app;
    let resp = create_router(app)
        .oneshot(authed(
            "GET",
            &format!("/Videos/{STACKED_ID}/AdditionalParts"),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = json["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Heist cd2", "Heist cd3"]);
    assert_eq!(json["TotalRecordCount"].as_i64(), Some(2));
    assert_eq!(json["StartIndex"].as_i64(), Some(0));
    assert_eq!(*owners.lock().unwrap(), [Some(STACKED_ID)]);
}

#[tokio::test]
async fn additional_parts_missing_item_is_404() {
    let (_dir, path) = temp_media();
    let app = state(&path, no_subtitles()).app;
    let router = create_router(app);
    let missing = Uuid::from_u128(0x00FF_FFFF);
    let resp = router
        .oneshot(authed("GET", &format!("/Videos/{missing}/AdditionalParts")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---- Subtitle upload / delete ----------------------------------------------

#[tokio::test]
async fn subtitle_delete_records_and_204() {
    let (_dir, path) = temp_media();
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let app = elevated_state(
        &path,
        Arc::new(CannedSubtitles {
            deleted: deleted.clone(),
        }),
    )
    .app;
    let (status, _) = call_with_body(
        app,
        "DELETE",
        &format!("/Videos/{ITEM_ID}/Subtitles/3"),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(*deleted.lock().unwrap(), vec![(ITEM_ID, 3)]);
}

#[tokio::test]
async fn subtitle_delete_missing_item_404() {
    let (_dir, path) = temp_media();
    let app = elevated_state(
        &path,
        Arc::new(CannedSubtitles {
            deleted: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .app;
    let (status, _) = call_with_body(
        app,
        "DELETE",
        &format!("/Videos/{}/Subtitles/0", Uuid::from_u128(7)),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn subtitle_upload_bad_base64_is_400() {
    let (_dir, path) = temp_media();
    let app = state(
        &path,
        Arc::new(CannedSubtitles {
            deleted: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .app;
    let body = serde_json::json!({
        "Language": "eng",
        "Format": "srt",
        "IsForced": false,
        "IsHearingImpaired": false,
        "Data": "!!!not-base64!!!"
    })
    .to_string();
    let (status, _) = call_with_body(
        app,
        "POST",
        &format!("/Videos/{ITEM_ID}/Subtitles"),
        Body::from(body),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
