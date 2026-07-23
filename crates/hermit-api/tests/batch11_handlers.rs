//! Batch-11 handler success/failure-path tests: Subtitles + Lyrics +
//! MediaSegments + Trickplay.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `hermit-traits` impls that authenticate and return canned data. Managers
//! a given handler never touches reuse the `test_support` panic fakes, catching a
//! handler that strays. The trickplay tile route serves a real temp file so the
//! `ServeFile` tail is covered.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeAppHost, FakeCollections, FakeConfig, FakeDto, FakeLyrics, FakeMediaSegments,
    FakeMediaSources, FakeMusic, FakePlaylists, FakeQuickConnect, FakeSearch, FakeSessions,
    FakeSimilarItems, FakeSubtitles, FakeSystem, FakeTrickplay, FakeUserData, FakeUserViews,
    minimal_base_item,
};
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::users::UserEntity;
use hermit_model::lyrics::{LyricDto, RemoteLyricInfoDto};
use hermit_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use hermit_model::providers::RemoteSubtitleInfo;
use hermit_traits::error::ServiceError;
use hermit_traits::library::LibraryManager;
use hermit_traits::media_segments::{MediaSegmentManager, MediaSegmentProviderInfo};
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::{
    AuthorizationInfo, DeleteOptions, InternalItemsQuery, InternalPeopleQuery,
};
use hermit_traits::stubs::LyricManager;
use hermit_traits::subtitles::{SubtitleManager, SubtitleResponse, SubtitleSearchRequest};
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
    ) -> Result<Vec<hermit_model::providers::LyricProviderInfo>, ServiceError> {
        Ok(Vec::new())
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
    ) -> Result<Vec<hermit_model::providers::SubtitleProviderInfo>, ServiceError> {
        Ok(Vec::new())
    }
}

/// Builds an [`AppState`] over the batch-11 managers, defaulting the untouched
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

// ---- MediaSegments ---------------------------------------------------------

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

// ---- Trickplay -------------------------------------------------------------

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

// ---- Lyrics ----------------------------------------------------------------

#[tokio::test]
async fn lyrics_get_returns_stored_or_404() {
    let mut dto = LyricDto::default();
    dto.lyrics.push(hermit_model::lyrics::LyricLine {
        text: "la la la".to_owned(),
        start: Some(0),
        cues: None,
    });
    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(CannedLyrics {
            stored: Some(dto),
            deleted: Arc::new(Mutex::new(false)),
        }),
        Arc::new(FakeSubtitles),
    );
    let (ok, body) = call(app, "GET", &format!("/Audio/{ITEM_ID}/Lyrics")).await;
    assert_eq!(ok, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Lyrics"][0]["Text"], "la la la");

    // No stored lyrics → 404.
    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(CannedLyrics {
            stored: None,
            deleted: Arc::new(Mutex::new(false)),
        }),
        Arc::new(FakeSubtitles),
    );
    let (missing, _) = call(app, "GET", &format!("/Audio/{ITEM_ID}/Lyrics")).await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn lyrics_delete_is_204_and_calls_manager() {
    let deleted = Arc::new(Mutex::new(false));
    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(CannedLyrics {
            stored: None,
            deleted: deleted.clone(),
        }),
        Arc::new(FakeSubtitles),
    );
    let (status, _) = call(app, "DELETE", &format!("/Audio/{ITEM_ID}/Lyrics")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(*deleted.lock().unwrap());
}

#[tokio::test]
async fn lyrics_remote_search_is_empty_list() {
    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(CannedLyrics {
            stored: None,
            deleted: Arc::new(Mutex::new(false)),
        }),
        Arc::new(FakeSubtitles),
    );
    let (status, body) = call(app, "GET", &format!("/Audio/{ITEM_ID}/RemoteSearch/Lyrics")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.as_array().unwrap().is_empty());
}

// ---- Subtitles -------------------------------------------------------------

#[tokio::test]
async fn subtitle_delete_records_and_204() {
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(FakeLyrics),
        Arc::new(CannedSubtitles {
            deleted: deleted.clone(),
        }),
    );
    let (status, _) = call(app, "DELETE", &format!("/Videos/{ITEM_ID}/Subtitles/3")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(*deleted.lock().unwrap(), vec![(ITEM_ID, 3)]);
}

#[tokio::test]
async fn subtitle_delete_missing_item_404() {
    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(FakeLyrics),
        Arc::new(CannedSubtitles {
            deleted: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let (status, _) = call(
        app,
        "DELETE",
        &format!("/Videos/{}/Subtitles/0", Uuid::from_u128(7)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn subtitle_remote_search_is_empty_list() {
    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(FakeLyrics),
        Arc::new(CannedSubtitles {
            deleted: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let (status, body) = call(
        app,
        "GET",
        &format!("/Items/{ITEM_ID}/RemoteSearch/Subtitles/eng"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn subtitle_upload_bad_base64_is_400() {
    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(FakeLyrics),
        Arc::new(CannedSubtitles {
            deleted: Arc::new(Mutex::new(Vec::new())),
        }),
    );
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

// ---- Intentionally-deferred routes stay on the 501 stub --------------------

#[tokio::test]
async fn deferred_routes_still_return_501() {
    let app = state(
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(FakeLyrics),
        Arc::new(FakeSubtitles),
    );
    // On-the-fly subtitle conversion + HLS subtitle playlist (SubtitleEncoder),
    // fallback fonts (encoding-options config not surfaced), and the plugin
    // SegmentEditor routes all remain unimplemented.
    for (method, uri) in [
        (
            "GET",
            format!("/Videos/{ITEM_ID}/msrc/Subtitles/0/Stream.vtt"),
        ),
        (
            "GET",
            format!("/Videos/{ITEM_ID}/msrc/Subtitles/0/subtitles.m3u8"),
        ),
        ("GET", "/FallbackFont/Fonts".to_owned()),
        ("GET", "/FallbackFont/Fonts/font.ttf".to_owned()),
        // The plugin SegmentEditor create route (POST-only in the contract).
        ("POST", format!("/MediaSegmentsApi/{ITEM_ID}")),
    ] {
        let (status, _) = call(app.clone(), method, &uri).await;
        assert_eq!(
            status,
            StatusCode::NOT_IMPLEMENTED,
            "expected 501 for deferred route {method} {uri}"
        );
    }
}
