//! Handler tests for the HLS / dynamic-transcode routes (`handlers::hls`).
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with a
//! fake [`HlsStreamManager`] / [`AttachmentExtractor`] injected via
//! [`AppState::with_media_encoding`], plus an OK auth stub. The playlist routes
//! assert the `.m3u8` content type + body; the segment/legacy routes serve a real
//! temp file so the `ServeFile` path is exercised and the seam's content type
//! override lands; the attachment route asserts the bytes + MIME type; the
//! `DELETE /Videos/ActiveEncodings` route asserts the recorded stop call.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::minimal_base_item;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::entities_media::MediaAttachment;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::media_encoding::{
    AttachmentExtractor, ExtractedAttachment, HlsStreamManager, HlsStreamRequest, ServedFile,
};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{AuthorizationInfo, InternalItemsQuery};
use tower::ServiceExt;
use uuid::Uuid;

const ITEM_ID: Uuid = Uuid::from_u128(0x00B1_0001);

/// The last request the fake HLS manager was asked to serve (for assertions).
#[derive(Default)]
struct Recorded {
    last: Mutex<Option<HlsStreamRequest>>,
    stopped: Mutex<Vec<(Option<String>, Option<String>)>>,
    resolved: Mutex<Vec<(String, bool)>>,
}

/// A fake [`HlsStreamManager`] that returns canned playlists and serves a real
/// temp file for segment/legacy routes.
struct FakeHls {
    /// The temp file every segment/legacy/transcode route serves.
    served_path: String,
    rec: Arc<Recorded>,
    /// When set, every method fails with this error (drives the error paths).
    fail: Option<ServiceError>,
}

impl FakeHls {
    fn served(&self, ct: &str) -> Result<ServedFile, ServiceError> {
        if let Some(e) = &self.fail {
            return Err(clone_err(e));
        }
        Ok(ServedFile {
            path: self.served_path.clone(),
            content_type: ct.to_owned(),
        })
    }
    fn playlist(&self, body: &str) -> Result<String, ServiceError> {
        if let Some(e) = &self.fail {
            return Err(clone_err(e));
        }
        Ok(body.to_owned())
    }
}

/// `ServiceError` is not `Clone`; reproduce the variant we use in tests.
fn clone_err(e: &ServiceError) -> ServiceError {
    match e {
        ServiceError::NotFound(m) => ServiceError::NotFound(m.clone()),
        ServiceError::InvalidInput(m) => ServiceError::InvalidInput(m.clone()),
        other => ServiceError::NotFound(format!("{other:?}")),
    }
}

#[async_trait]
impl HlsStreamManager for FakeHls {
    async fn master_playlist(
        &self,
        request: &HlsStreamRequest,
        _is_audio: bool,
    ) -> Result<String, ServiceError> {
        *self.rec.last.lock().unwrap() = Some(request.clone());
        self.playlist("#EXTM3U\n#MASTER\n")
    }

    async fn variant_playlist(
        &self,
        request: &HlsStreamRequest,
        _is_audio: bool,
    ) -> Result<String, ServiceError> {
        *self.rec.last.lock().unwrap() = Some(request.clone());
        self.playlist("#EXTM3U\n#VARIANT\n")
    }

    async fn live_playlist(&self, request: &HlsStreamRequest) -> Result<String, ServiceError> {
        *self.rec.last.lock().unwrap() = Some(request.clone());
        self.playlist("#EXTM3U\n#LIVE\n")
    }

    async fn dynamic_segment(
        &self,
        request: &HlsStreamRequest,
        _segment_id: i32,
        _is_audio: bool,
    ) -> Result<ServedFile, ServiceError> {
        *self.rec.last.lock().unwrap() = Some(request.clone());
        self.served("video/mp2t")
    }

    async fn resolve_transcode_file(
        &self,
        file_name: &str,
        require_m3u8: bool,
    ) -> Result<ServedFile, ServiceError> {
        self.rec
            .resolved
            .lock()
            .unwrap()
            .push((file_name.to_owned(), require_m3u8));
        self.served(if require_m3u8 {
            "application/vnd.apple.mpegurl"
        } else {
            "video/mp2t"
        })
    }

    async fn transcode_stream(
        &self,
        request: &HlsStreamRequest,
        _is_audio: bool,
    ) -> Result<ServedFile, ServiceError> {
        *self.rec.last.lock().unwrap() = Some(request.clone());
        self.served("video/mp4")
    }

    async fn stop_encoding(&self, request: &HlsStreamRequest) -> Result<(), ServiceError> {
        if let Some(e) = &self.fail {
            return Err(clone_err(e));
        }
        self.rec
            .stopped
            .lock()
            .unwrap()
            .push((request.device_id.clone(), request.play_session_id.clone()));
        Ok(())
    }

    async fn ping_transcoding_job(
        &self,
        play_session_id: &str,
        _is_user_paused: Option<bool>,
    ) -> Result<(), ServiceError> {
        if let Some(e) = &self.fail {
            return Err(clone_err(e));
        }
        if play_session_id.trim().is_empty() {
            return Err(ServiceError::invalid_input("playSessionId is empty"));
        }
        Ok(())
    }
}

/// A fake [`AttachmentExtractor`] returning fixed bytes + MIME.
struct FakeAttachments {
    mime: Option<String>,
    data: Vec<u8>,
}

#[async_trait]
impl AttachmentExtractor for FakeAttachments {
    async fn get_attachment(
        &self,
        _item_id: Uuid,
        _media_source_id: &str,
        _attachment_stream_index: i32,
    ) -> Result<ExtractedAttachment, ServiceError> {
        Ok(ExtractedAttachment {
            attachment: MediaAttachment {
                codec: None,
                codec_tag: None,
                comment: None,
                index: 0,
                file_name: Some("font.ttf".to_owned()),
                mime_type: self.mime.clone(),
                delivery_url: None,
            },
            data: self.data.clone(),
        })
    }

    async fn extract_all_attachments(
        &self,
        _input_file: &str,
        _media_source: &ferrofin_model::dto::MediaSourceInfo,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// An [`AuthService`]/[`AuthorizationContext`] that authenticates any request.
struct OkAuth;

#[async_trait]
impl AuthorizationContext for OkAuth {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(ok_auth_info())
    }
}

/// The authorization every request resolves to: authenticated, carrying the
/// presented token (the master playlist embeds it as `ApiKey`).
fn ok_auth_info() -> AuthorizationInfo {
    AuthorizationInfo {
        token: Some(ferrofin_model::secret::Secret::new("token")),
        is_authenticated: true,
        ..AuthorizationInfo::default()
    }
}

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(ok_auth_info())
    }
}

/// A [`LibraryManager`] whose `get_item_by_id` returns a present item (or `None`
/// when `present` is false), used only by the attachment route.
struct ItemLibrary {
    present: bool,
}

#[async_trait]
impl LibraryManager for ItemLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok(self.present.then(|| minimal_base_item(id, "clip", "Movie")))
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
    async fn delete_item(
        &self,
        _id: Uuid,
        _options: &ferrofin_traits::options::DeleteOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn get_people(
        &self,
        _query: &ferrofin_traits::options::InternalPeopleQuery,
    ) -> Result<Vec<ferrofin_db::entities::base_items::PeopleEntity>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_people_names(
        &self,
        _query: &ferrofin_traits::options::InternalPeopleQuery,
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
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_studios(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_music_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_album_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
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

/// An RAII temp file the segment/legacy routes serve.
struct TempFile {
    path: String,
}
impl TempFile {
    fn new() -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!("ferrofin-hls-{}.ts", Uuid::new_v4()));
        std::fs::write(&p, b"SEGMENT-BYTES").unwrap();
        Self {
            path: p.to_string_lossy().into_owned(),
        }
    }
}
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A [`MediaSourceManager`] whose static sources are empty, so the direct-play
/// handlers `404` and fall through to the transcode branch.
struct NoStaticSources;

#[async_trait]
impl ferrofin_traits::library::MediaSourceManager for NoStaticSources {
    async fn get_media_streams(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::entities_media::MediaStream>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_media_attachments(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<MediaAttachment>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_playback_media_sources(
        &self,
        _item_id: Uuid,
        _user_id: Uuid,
        _allow_media_probe: bool,
        _enable_path_substitution: bool,
    ) -> Result<Vec<ferrofin_model::dto::MediaSourceInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_static_media_sources(
        &self,
        _item_id: Uuid,
        _enable_path_substitution: bool,
        _user_id: Option<Uuid>,
    ) -> Result<Vec<ferrofin_model::dto::MediaSourceInfo>, ServiceError> {
        // No direct-playable source → the handler's `404` branch → transcode.
        Ok(Vec::new())
    }
    async fn open_live_stream(
        &self,
        _request: &ferrofin_model::media_info::LiveStreamRequest,
    ) -> Result<ferrofin_model::dto::MediaSourceInfo, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_live_stream(
        &self,
        _id: &str,
    ) -> Result<ferrofin_model::dto::MediaSourceInfo, ServiceError> {
        unimplemented!("unused")
    }
    async fn refresh_media_streams(&self, _item_id: uuid::Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
}

/// Builds a router whose HLS seam is `hls` and library is `library`, with OK auth.
fn router_with(
    hls: Arc<dyn HlsStreamManager>,
    attachments: Arc<dyn AttachmentExtractor>,
    library: Arc<dyn LibraryManager>,
) -> axum::Router {
    create_router(build_state(hls, attachments, library))
}

/// Like [`router_with`] but with a media-source manager that yields no static
/// sources, so the direct-play stream routes fall through to transcode.
fn router_no_static(hls: Arc<dyn HlsStreamManager>) -> axum::Router {
    use ferrofin_api::test_support as t;
    let app = AppState::new(
        Arc::new(ItemLibrary { present: true }),
        Arc::new(t::FakeUsers),
        Arc::new(t::FakeUserViews),
        Arc::new(t::FakeUserData),
        Arc::new(NoStaticSources),
        Arc::new(t::FakeSessions),
        Arc::new(t::FakeSystem),
        Arc::new(t::FakeAppHost),
        Arc::new(t::FakeConfig),
        Arc::new(t::FakeProviders),
        Arc::new(t::FakeMusic),
        Arc::new(t::FakeSimilarItems),
        Arc::new(t::FakeSearch),
        Arc::new(t::FakeDto),
        Arc::new(OkAuth),
        Arc::new(OkAuth),
        Arc::new(t::FakeQuickConnect),
        Arc::new(t::FakePlaylists),
        Arc::new(t::FakeCollections),
        Arc::new(t::FakeTvSeries),
        Arc::new(t::FakeSubtitles),
        Arc::new(t::FakeLyrics),
        Arc::new(t::FakeMediaSegments),
        Arc::new(t::FakeTrickplay),
        Arc::new(t::FakeDevices),
        Arc::new(t::FakeClientEventLogger),
        Arc::new(t::FakeApiKeys),
        Arc::new(t::FakeLocalization),
        Arc::new(t::FakeDisplayPreferences),
        Arc::new(t::FakeActivity),
        Arc::new(t::FakeFileSystem),
        Arc::new(t::FakeTasks),
    )
    .with_media_encoding(
        hls,
        Arc::new(FakeAttachments {
            mime: None,
            data: vec![],
        }),
    );
    create_router(app)
}

/// Assembles an [`AppState`] with OK auth, the given library, and the media
/// seams; every other manager is a panicking fake (unused by these routes).
fn build_state(
    hls: Arc<dyn HlsStreamManager>,
    attachments: Arc<dyn AttachmentExtractor>,
    library: Arc<dyn LibraryManager>,
) -> AppState {
    use ferrofin_api::test_support as t;
    AppState::new(
        library,
        Arc::new(t::FakeUsers),
        Arc::new(t::FakeUserViews),
        Arc::new(t::FakeUserData),
        Arc::new(t::FakeMediaSources),
        Arc::new(t::FakeSessions),
        Arc::new(t::FakeSystem),
        Arc::new(t::FakeAppHost),
        Arc::new(t::FakeConfig),
        Arc::new(t::FakeProviders),
        Arc::new(t::FakeMusic),
        Arc::new(t::FakeSimilarItems),
        Arc::new(t::FakeSearch),
        Arc::new(t::FakeDto),
        Arc::new(OkAuth),
        Arc::new(OkAuth),
        Arc::new(t::FakeQuickConnect),
        Arc::new(t::FakePlaylists),
        Arc::new(t::FakeCollections),
        Arc::new(t::FakeTvSeries),
        Arc::new(t::FakeSubtitles),
        Arc::new(t::FakeLyrics),
        Arc::new(t::FakeMediaSegments),
        Arc::new(t::FakeTrickplay),
        Arc::new(t::FakeDevices),
        Arc::new(t::FakeClientEventLogger),
        Arc::new(t::FakeApiKeys),
        Arc::new(t::FakeLocalization),
        Arc::new(t::FakeDisplayPreferences),
        Arc::new(t::FakeActivity),
        Arc::new(t::FakeFileSystem),
        Arc::new(t::FakeTasks),
    )
    .with_media_encoding(hls, attachments)
}

/// An authorization context that rejects everything, so a route's auth gate is
/// observable: with [`OkAuth`] every extractor succeeds and a missing gate looks
/// identical to a present one.
struct DenyAuth;

#[async_trait]
impl AuthorizationContext for DenyAuth {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo::default())
    }
}

#[async_trait]
impl AuthService for DenyAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Err(ServiceError::unauthorized("no token"))
    }
}

/// Like [`build_state`] but with an auth service that rejects every request, so
/// a route's auth gate becomes observable.
fn router_denying_auth(hls: Arc<dyn HlsStreamManager>) -> axum::Router {
    use ferrofin_api::test_support as t;
    let app = AppState::new(
        Arc::new(ItemLibrary { present: true }),
        Arc::new(t::FakeUsers),
        Arc::new(t::FakeUserViews),
        Arc::new(t::FakeUserData),
        Arc::new(t::FakeMediaSources),
        Arc::new(t::FakeSessions),
        Arc::new(t::FakeSystem),
        Arc::new(t::FakeAppHost),
        Arc::new(t::FakeConfig),
        Arc::new(t::FakeProviders),
        Arc::new(t::FakeMusic),
        Arc::new(t::FakeSimilarItems),
        Arc::new(t::FakeSearch),
        Arc::new(t::FakeDto),
        Arc::new(DenyAuth),
        Arc::new(DenyAuth),
        Arc::new(t::FakeQuickConnect),
        Arc::new(t::FakePlaylists),
        Arc::new(t::FakeCollections),
        Arc::new(t::FakeTvSeries),
        Arc::new(t::FakeSubtitles),
        Arc::new(t::FakeLyrics),
        Arc::new(t::FakeMediaSegments),
        Arc::new(t::FakeTrickplay),
        Arc::new(t::FakeDevices),
        Arc::new(t::FakeClientEventLogger),
        Arc::new(t::FakeApiKeys),
        Arc::new(t::FakeLocalization),
        Arc::new(t::FakeDisplayPreferences),
        Arc::new(t::FakeActivity),
        Arc::new(t::FakeFileSystem),
        Arc::new(t::FakeTasks),
    )
    .with_media_encoding(
        hls,
        Arc::new(FakeAttachments {
            mime: None,
            data: vec![],
        }),
    );
    create_router(app)
}

fn ok_hls(served_path: &str, rec: Arc<Recorded>) -> Arc<FakeHls> {
    Arc::new(FakeHls {
        served_path: served_path.to_owned(),
        rec,
        fail: None,
    })
}

fn authed(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", "Bearer token")
        .body(Body::empty())
        .unwrap()
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn video_master_playlist_returns_m3u8() {
    let rec = Arc::new(Recorded::default());
    let router = router_with(
        ok_hls("/nonexistent", rec.clone()),
        Arc::new(FakeAttachments {
            mime: None,
            data: vec![],
        }),
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed(
            "GET",
            &format!(
                "/Videos/{ITEM_ID}/master.m3u8?deviceId=dev1&playSessionId=s1&mediaSourceId=src9"
            ),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/vnd.apple.mpegurl"
    );
    let last = rec.last.lock().unwrap().clone().unwrap();
    assert_eq!(last.item_id, ITEM_ID);
    assert_eq!(last.device_id.as_deref(), Some("dev1"));
    assert_eq!(last.play_session_id.as_deref(), Some("s1"));
    assert_eq!(last.media_source_id.as_deref(), Some("src9"));
    assert!(last.query_string.starts_with('?'));
    // The HTTP-context inputs `DynamicHlsHelper` reads: the session token (for
    // the subtitle/trickplay `ApiKey`), the peer locality (none in a oneshot
    // → not local), and the master route's DTO defaults.
    assert_eq!(last.api_key.as_deref(), Some("token"));
    assert!(!last.is_in_local_network);
    assert!(!last.enable_subtitles_in_manifest);
    assert!(!last.enable_adaptive_bitrate_streaming);
    assert!(last.enable_trickplay);
    // `GetMasterPlaylistInternal` stamps `Expires: 0` on every master response.
    assert_eq!(resp.headers().get("expires").unwrap(), "0");
    let body = body_string(resp).await;
    assert!(body.contains("#MASTER"));
}

/// The contract spells the caps `videoBitRate`/`audioBitRate` and the manifest
/// flags `enableAdaptiveBitrateStreaming`/`enableTrickplay`; all reach the seam.
#[tokio::test]
async fn video_master_playlist_parses_contract_spellings() {
    let rec = Arc::new(Recorded::default());
    let router = router_with(
        ok_hls("/nonexistent", rec.clone()),
        Arc::new(FakeAttachments {
            mime: None,
            data: vec![],
        }),
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed(
            "GET",
            &format!(
                "/Videos/{ITEM_ID}/master.m3u8?videoBitRate=1000000&audioBitRate=128000&\
                 enableAdaptiveBitrateStreaming=true&enableTrickplay=false&\
                 enableSubtitlesInManifest=true&subtitleStreamIndex=3&subtitleMethod=Hls&\
                 profile=high&level=41&framerate=30&width=640&height=360&minSegments=2&\
                 transcodeReasons=ContainerNotSupported"
            ),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let last = rec.last.lock().unwrap().clone().unwrap();
    assert_eq!(last.video_bitrate, Some(1_000_000));
    assert_eq!(last.audio_bitrate, Some(128_000));
    assert!(last.enable_adaptive_bitrate_streaming);
    assert!(!last.enable_trickplay);
    assert!(last.enable_subtitles_in_manifest);
    assert_eq!(last.subtitle_stream_index, Some(3));
    assert_eq!(last.subtitle_method.as_deref(), Some("Hls"));
    assert_eq!(last.profile.as_deref(), Some("high"));
    assert_eq!(last.level.as_deref(), Some("41"));
    assert_eq!(last.framerate, Some(30.0));
    assert_eq!((last.width, last.height), (Some(640), Some(360)));
    assert_eq!(last.min_segments, Some(2));
    assert_eq!(
        last.transcode_reasons.as_deref(),
        Some("ContainerNotSupported")
    );
}

#[tokio::test]
async fn video_master_head_is_ok() {
    let rec = Arc::new(Recorded::default());
    let router = router_with(
        ok_hls("/nope", rec),
        Arc::new(FakeAttachments {
            mime: None,
            data: vec![],
        }),
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed("HEAD", &format!("/Videos/{ITEM_ID}/master.m3u8")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // A HEAD answers with the playlist MIME, `Expires: 0`, and no body
    // (`new FileContentResult(Array.Empty<byte>(), playlist mime)`).
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/vnd.apple.mpegurl"
    );
    assert_eq!(resp.headers().get("expires").unwrap(), "0");
    assert_eq!(body_string(resp).await, "");
}

#[tokio::test]
async fn video_main_and_live_and_audio_playlists() {
    let rec = Arc::new(Recorded::default());
    let attachments: Arc<dyn AttachmentExtractor> = Arc::new(FakeAttachments {
        mime: None,
        data: vec![],
    });
    for (uri, marker) in [
        (format!("/Videos/{ITEM_ID}/main.m3u8"), "#VARIANT"),
        (format!("/Videos/{ITEM_ID}/live.m3u8"), "#LIVE"),
        (format!("/Audio/{ITEM_ID}/master.m3u8"), "#MASTER"),
        (format!("/Audio/{ITEM_ID}/main.m3u8"), "#VARIANT"),
    ] {
        let router = router_with(
            ok_hls("/nope", rec.clone()),
            attachments.clone(),
            Arc::new(ItemLibrary { present: true }),
        );
        let resp = router.oneshot(authed("GET", &uri)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "uri {uri}");
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/vnd.apple.mpegurl"
        );
        assert!(body_string(resp).await.contains(marker), "uri {uri}");
        // `GetLiveHlsStream` defaults `EnableSubtitlesInManifest` to true;
        // every other route's DTO defaults it to false.
        let last = rec.last.lock().unwrap().clone().unwrap();
        assert_eq!(
            last.enable_subtitles_in_manifest,
            uri.ends_with("live.m3u8"),
            "uri {uri}"
        );
    }
}

#[tokio::test]
async fn dynamic_video_segment_serves_file_with_seam_content_type() {
    let tmp = TempFile::new();
    let rec = Arc::new(Recorded::default());
    let router = router_with(
        ok_hls(&tmp.path, rec),
        Arc::new(FakeAttachments {
            mime: None,
            data: vec![],
        }),
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/hls1/main/3.ts?runtimeTicks=0"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("content-type").unwrap(), "video/mp2t");
    assert_eq!(body_string(resp).await, "SEGMENT-BYTES");
}

#[tokio::test]
async fn dynamic_audio_segment_serves_file() {
    let tmp = TempFile::new();
    let rec = Arc::new(Recorded::default());
    let router = router_with(
        ok_hls(&tmp.path, rec),
        Arc::new(FakeAttachments {
            mime: None,
            data: vec![],
        }),
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed("GET", &format!("/Audio/{ITEM_ID}/hls1/main/0.aac")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_string(resp).await, "SEGMENT-BYTES");
}

#[tokio::test]
async fn bad_segment_id_is_400() {
    let rec = Arc::new(Recorded::default());
    let router = router_with(
        ok_hls("/nope", rec),
        Arc::new(FakeAttachments {
            mime: None,
            data: vec![],
        }),
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/hls1/main/notanint"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn legacy_video_segment_and_playlist_and_audio() {
    let tmp = TempFile::new();
    let rec = Arc::new(Recorded::default());
    let attachments: Arc<dyn AttachmentExtractor> = Arc::new(FakeAttachments {
        mime: None,
        data: vec![],
    });
    // Legacy video segment.
    let router = router_with(
        ok_hls(&tmp.path, rec.clone()),
        attachments.clone(),
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed("GET", &format!("/Videos/{ITEM_ID}/hls/pl7/9.ts")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Legacy playlist (requires m3u8).
    let router = router_with(
        ok_hls(&tmp.path, rec.clone()),
        attachments.clone(),
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/hls/pl7/stream.m3u8"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Legacy audio segment (.mp3).
    let router = router_with(
        ok_hls(&tmp.path, rec.clone()),
        attachments,
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed(
            "GET",
            &format!("/Audio/{ITEM_ID}/hls/seg2/stream.mp3"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resolved = rec.resolved.lock().unwrap();
    // The video segment reconstructs `<segmentId><ext>`; the playlist requires m3u8.
    assert!(resolved.iter().any(|(n, m)| n == "9.ts" && !m));
    assert!(resolved.iter().any(|(n, m)| n == "pl7.m3u8" && *m));
    assert!(resolved.iter().any(|(n, m)| n == "seg2.mp3" && !m));
}

#[tokio::test]
async fn stop_encoding_returns_204_and_records() {
    let rec = Arc::new(Recorded::default());
    let router = router_with(
        ok_hls("/nope", rec.clone()),
        Arc::new(FakeAttachments {
            mime: None,
            data: vec![],
        }),
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed(
            "DELETE",
            "/Videos/ActiveEncodings?deviceId=devX&playSessionId=psY",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let stopped = rec.stopped.lock().unwrap();
    assert_eq!(
        stopped[0],
        (Some("devX".to_owned()), Some("psY".to_owned()))
    );
}

#[tokio::test]
async fn attachment_serves_bytes_with_mime() {
    let rec = Arc::new(Recorded::default());
    let router = router_with(
        ok_hls("/nope", rec),
        Arc::new(FakeAttachments {
            mime: Some("font/ttf".to_owned()),
            data: b"FONTDATA".to_vec(),
        }),
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/src1/Attachments/2"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("content-type").unwrap(), "font/ttf");
    assert_eq!(body_string(resp).await, "FONTDATA");
}

#[tokio::test]
async fn attachment_defaults_octet_stream_when_no_mime() {
    let rec = Arc::new(Recorded::default());
    let router = router_with(
        ok_hls("/nope", rec),
        Arc::new(FakeAttachments {
            mime: None,
            data: b"X".to_vec(),
        }),
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/src1/Attachments/0"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );
}

#[tokio::test]
async fn attachment_missing_item_is_404() {
    let rec = Arc::new(Recorded::default());
    let router = router_with(
        ok_hls("/nope", rec),
        Arc::new(FakeAttachments {
            mime: None,
            data: vec![],
        }),
        Arc::new(ItemLibrary { present: false }),
    );
    let resp = router
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/src1/Attachments/0"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn playlist_seam_error_maps_to_status() {
    // A seam NotFound (disabled runtime) surfaces as 404 through ApiError.
    let hls: Arc<dyn HlsStreamManager> = Arc::new(FakeHls {
        served_path: "/nope".to_owned(),
        rec: Arc::new(Recorded::default()),
        fail: Some(ServiceError::NotFound("no runtime".to_owned())),
    });
    let router = router_with(
        hls,
        Arc::new(FakeAttachments {
            mime: None,
            data: vec![],
        }),
        Arc::new(ItemLibrary { present: true }),
    );
    let resp = router
        .oneshot(authed("GET", &format!("/Videos/{ITEM_ID}/master.m3u8")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn video_stream_container_falls_back_to_transcode() {
    let tmp = TempFile::new();
    let rec = Arc::new(Recorded::default());
    let router = router_no_static(ok_hls(&tmp.path, rec.clone()));
    // No static source → direct-play 404 → transcode_stream serves the file.
    let resp = router
        .oneshot(authed(
            "GET",
            &format!("/Videos/{ITEM_ID}/stream.mp4?deviceId=d1"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("content-type").unwrap(), "video/mp4");
    assert_eq!(body_string(resp).await, "SEGMENT-BYTES");
    let last = rec.last.lock().unwrap().clone().unwrap();
    assert_eq!(last.item_id, ITEM_ID);
    assert_eq!(last.device_id.as_deref(), Some("d1"));
}

#[tokio::test]
async fn audio_universal_falls_back_to_transcode() {
    let tmp = TempFile::new();
    let rec = Arc::new(Recorded::default());
    let router = router_no_static(ok_hls(&tmp.path, rec));
    let resp = router
        .oneshot(authed("GET", &format!("/Audio/{ITEM_ID}/universal")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_string(resp).await, "SEGMENT-BYTES");
}

#[tokio::test]
async fn hls1_segments_require_auth_but_legacy_segments_stay_open() {
    // Upstream `DynamicHlsController` carries a class-level `[Authorize]` with no
    // `[AllowAnonymous]`, so the `hls1` segment routes are authenticated in
    // Jellyfin. The LEGACY `hls` segment routes are deliberately anonymous
    // upstream — `HlsSegmentController` says so in a comment ("Can't require
    // authentication just yet due to seeing some requests come from Chrome
    // without full query string"). Both halves are pinned here: gating the
    // legacy routes would be as much a parity break as leaving hls1 open.
    let tmp = TempFile::new();
    let rec = Arc::new(Recorded::default());
    let router = router_denying_auth(ok_hls(&tmp.path, rec));

    for uri in [
        format!("/Videos/{ITEM_ID}/hls1/main/3.ts?runtimeTicks=0"),
        format!("/Audio/{ITEM_ID}/hls1/main/0.aac"),
    ] {
        let resp = router.clone().oneshot(authed("GET", &uri)).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "hls1 segment {uri} must be gated"
        );
    }

    let legacy = format!("/Videos/{ITEM_ID}/hls/main/3.ts");
    let resp = router.oneshot(authed("GET", &legacy)).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "legacy hls segments are anonymous upstream — gating them breaks parity"
    );
}
