//! `GET /Audio/{itemId}/universal` tests: the device-profile negotiation of
//! `UniversalAudioController.GetUniversalAudioStream` and its four outcomes
//! (remote redirect, static file, HLS master playlist, progressive transcode).
//!
//! Each test drives the real handler through `tower::ServiceExt::oneshot` with a
//! FLAC (or remote) media source, an `Audio` item and a fake
//! [`HlsStreamManager`] that records the request it was handed.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support as t;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::dto::{MediaSourceInfo, MediaSourceType};
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::{MediaAttachment, MediaStream};
use ferrofin_model::media_info::{LiveStreamRequest, MediaProtocol};
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, MediaSourceManager};
use ferrofin_traits::media_encoding::{
    AttachmentExtractor, ExtractedAttachment, HlsStreamManager, HlsStreamRequest, ServedFile,
};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{AuthorizationInfo, InternalItemsQuery};
use tower::ServiceExt;
use uuid::Uuid;

const ITEM_ID: Uuid = Uuid::from_u128(0x00C1_0001);

/// Which seam method served the request, and with what.
#[derive(Debug, Clone)]
enum Served {
    Master(HlsStreamRequest),
    Progressive(HlsStreamRequest),
}

/// A fake [`HlsStreamManager`]: a canned master playlist, a progressive
/// transcode served from a temp file, and the request each was handed.
struct FakeHls {
    path: String,
    served: Arc<Mutex<Option<Served>>>,
}

#[async_trait]
impl HlsStreamManager for FakeHls {
    async fn master_playlist(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
    ) -> Result<String, ServiceError> {
        assert!(is_audio, "the universal master is an audio master");
        *self.served.lock().unwrap() = Some(Served::Master(request.clone()));
        Ok("#EXTM3U\n#MASTER\n".to_owned())
    }
    async fn variant_playlist(
        &self,
        _request: &HlsStreamRequest,
        _is_audio: bool,
    ) -> Result<String, ServiceError> {
        unimplemented!("unused")
    }
    async fn live_playlist(&self, _request: &HlsStreamRequest) -> Result<String, ServiceError> {
        unimplemented!("unused")
    }
    async fn dynamic_segment(
        &self,
        _request: &HlsStreamRequest,
        _segment_id: i32,
        _is_audio: bool,
    ) -> Result<ServedFile, ServiceError> {
        unimplemented!("unused")
    }
    async fn resolve_transcode_file(
        &self,
        _file_name: &str,
        _require_m3u8: bool,
    ) -> Result<ServedFile, ServiceError> {
        unimplemented!("unused")
    }
    async fn transcode_stream(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
    ) -> Result<ServedFile, ServiceError> {
        assert!(is_audio, "the universal transcode is an audio transcode");
        *self.served.lock().unwrap() = Some(Served::Progressive(request.clone()));
        Ok(ServedFile {
            path: self.path.clone(),
            content_type: "audio/mpeg".to_owned(),
            progress: None,
        })
    }
    async fn stop_encoding(&self, _request: &HlsStreamRequest) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn ping_transcoding_job(
        &self,
        _play_session_id: &str,
        _is_user_paused: Option<bool>,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
}

/// An unused [`AttachmentExtractor`] (the media-encoding seam needs one).
struct NoAttachments;

#[async_trait]
impl AttachmentExtractor for NoAttachments {
    async fn get_attachment(
        &self,
        _item_id: Uuid,
        _media_source_id: &str,
        _attachment_stream_index: i32,
    ) -> Result<ExtractedAttachment, ServiceError> {
        unimplemented!("unused")
    }
    async fn extract_all_attachments(
        &self,
        _input_file: &str,
        _media_source: &MediaSourceInfo,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
}

/// Authenticates as the sample user, allowed to transcode audio or not.
struct AudioAuth {
    transcode: bool,
}

fn auth_info(transcode: bool) -> AuthorizationInfo {
    AuthorizationInfo {
        user_policy: Some(Arc::new(ferrofin_model::users::UserPolicy {
            enable_remote_access: true,
            enable_audio_playback_transcoding: transcode,
            enable_video_playback_transcoding: false,
            enable_playback_remuxing: false,
            ..ferrofin_model::users::UserPolicy::default()
        })),
        ..t::authenticated_user_info()
    }
}

#[async_trait]
impl AuthService for AudioAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(auth_info(self.transcode))
    }
}

#[async_trait]
impl AuthorizationContext for AudioAuth {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(auth_info(self.transcode))
    }
}

/// Resolves [`ITEM_ID`] as a visible `Audio` item.
struct AudioLibrary;

#[async_trait]
impl LibraryManager for AudioLibrary {
    async fn is_item_visible_standalone(
        &self,
        _item: &BaseItemEntity,
        _user: &ferrofin_db::entities::users::UserEntity,
    ) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn is_item_visible(
        &self,
        _item: &BaseItemEntity,
        _user: &ferrofin_db::entities::users::UserEntity,
    ) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((id == ITEM_ID).then(|| BaseItemEntity {
            media_type: Some("Audio".to_owned()),
            ..t::minimal_base_item(ITEM_ID, "A Song", "Audio")
        }))
    }
    async fn query_items(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_item_ids(&self, _q: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_item_list(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_latest_item_list(
        &self,
        _q: &InternalItemsQuery,
        _c: ferrofin_model::data::CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("unused")
    }
    async fn create_items(
        &self,
        _i: &[BaseItemEntity],
        _p: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn update_items(
        &self,
        _i: &[BaseItemEntity],
        _p: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn delete_item(
        &self,
        _id: Uuid,
        _o: &ferrofin_traits::options::DeleteOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn get_people(
        &self,
        _q: &ferrofin_traits::options::InternalPeopleQuery,
    ) -> Result<Vec<ferrofin_db::entities::base_items::PeopleEntity>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_people_names(
        &self,
        _q: &ferrofin_traits::options::InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_count(&self, _q: &InternalItemsQuery) -> Result<i32, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_item_counts(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_studios(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_music_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_album_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_query_filters_legacy(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: MediaStreamType,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!("unused")
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
}

/// Serves the given sources as both the playback and the static sources.
struct Sources(Vec<MediaSourceInfo>);

#[async_trait]
impl MediaSourceManager for Sources {
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
        Ok(self.0.clone())
    }
    async fn get_static_media_sources(
        &self,
        _item_id: Uuid,
        _enable_path_substitution: bool,
        _user_id: Option<Uuid>,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        Ok(self.0.clone())
    }
    async fn open_live_stream(
        &self,
        _request: &LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_live_stream(&self, _id: &str) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!("unused")
    }
    async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
}

/// A temp file standing in for both the track and the transcode output.
struct TempFile {
    _dir: tempfile::TempDir,
    path: String,
}

fn temp_file() -> TempFile {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("track.flac");
    std::fs::write(&path, b"FLAC-BYTES").expect("write");
    TempFile {
        path: path.to_string_lossy().into_owned(),
        _dir: dir,
    }
}

/// A 16-bit/44.1 kHz stereo FLAC file at `path`, `bitrate` bit/s overall.
fn flac_source(id: Uuid, path: &str, bitrate: i32) -> MediaSourceInfo {
    MediaSourceInfo {
        id: Some(id.simple().to_string()),
        path: Some(path.to_owned()),
        protocol: MediaProtocol::File,
        type_: MediaSourceType::Default,
        container: Some("flac".to_owned()),
        bitrate: Some(bitrate),
        supports_direct_play: true,
        supports_direct_stream: true,
        supports_transcoding: true,
        media_streams: vec![MediaStream {
            stream_type: MediaStreamType::Audio,
            codec: Some("flac".to_owned()),
            channels: Some(2),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            bit_rate: Some(bitrate),
            index: 0,
            ..MediaStream::default()
        }],
        ..MediaSourceInfo::default()
    }
}

/// Builds the router around `sources`, returning it with the record of what
/// the HLS seam served.
fn router(
    sources: Vec<MediaSourceInfo>,
    transcode_path: &str,
) -> (axum::Router, Arc<Mutex<Option<Served>>>) {
    router_as(sources, transcode_path, true)
}

/// [`router`] for a user whose `EnableAudioPlaybackTranscoding` is `transcode`.
fn router_as(
    sources: Vec<MediaSourceInfo>,
    transcode_path: &str,
    transcode: bool,
) -> (axum::Router, Arc<Mutex<Option<Served>>>) {
    let auth = Arc::new(AudioAuth { transcode });
    let served = Arc::new(Mutex::new(None));
    let state = AppState::new(
        Arc::new(AudioLibrary),
        Arc::new(t::FakeUsers),
        Arc::new(t::FakeUserViews),
        Arc::new(t::FakeUserData),
        Arc::new(Sources(sources)),
        Arc::new(t::FakeSessions),
        Arc::new(t::FakeSystem),
        Arc::new(t::FakeAppHost),
        Arc::new(t::FakeConfig),
        Arc::new(t::FakeProviders),
        Arc::new(t::FakeMusic),
        Arc::new(t::FakeSimilarItems),
        Arc::new(t::FakeSearch),
        Arc::new(t::FakeDto),
        auth.clone(),
        auth,
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
        Arc::new(FakeHls {
            path: transcode_path.to_owned(),
            served: Arc::clone(&served),
        }),
        Arc::new(NoAttachments),
    );
    (create_router(state), served)
}

async fn get(router: axum::Router, query: &str) -> axum::response::Response {
    send(router, "GET", query).await
}

async fn send(router: axum::Router, method: &str, query: &str) -> axum::response::Response {
    let request = Request::builder()
        .method(method)
        .uri(format!("/Audio/{ITEM_ID}/universal?{query}"))
        .header("authorization", "MediaBrowser Token=\"t\"")
        .body(Body::empty())
        .unwrap();
    router.oneshot(request).await.expect("response")
}

fn progressive(served: &Mutex<Option<Served>>) -> HlsStreamRequest {
    match served.lock().unwrap().clone() {
        Some(Served::Progressive(request)) => request,
        other => panic!("expected a progressive transcode, got {other:?}"),
    }
}

fn master(served: &Mutex<Option<Served>>) -> HlsStreamRequest {
    match served.lock().unwrap().clone() {
        Some(Served::Master(request)) => request,
        other => panic!("expected an HLS master playlist, got {other:?}"),
    }
}

/// A container the client lists is played as the original file.
#[tokio::test]
async fn a_listed_container_is_served_as_the_original_file() {
    let file = temp_file();
    let (app, served) = router(vec![flac_source(ITEM_ID, &file.path, 900_000)], &file.path);
    let resp = get(app, "container=mp3,flac&maxStreamingBitrate=140000000").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::ACCEPT_RANGES], "bytes");
    assert!(served.lock().unwrap().is_none(), "nothing was transcoded");
}

/// The issue's case: a FLAC file for a client that lists only mp3/aac is a
/// progressive transcode to the negotiated container, carrying upstream's
/// `StreamingRequestDto` fields.
#[tokio::test]
async fn an_unlisted_container_is_transcoded_progressively() {
    let file = temp_file();
    let (app, served) = router(vec![flac_source(ITEM_ID, &file.path, 900_000)], &file.path);
    let resp = get(
        app,
        "container=mp3,aac&transcodingContainer=mp3&audioCodec=mp3&maxStreamingBitrate=128000\
         &maxAudioChannels=2&maxAudioSampleRate=48000&maxAudioBitDepth=16&deviceId=dev1\
         &startTimeTicks=50000000&static=true",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::ACCEPT_RANGES], "none");
    let req = progressive(&served);
    assert_eq!(req.item_id, ITEM_ID);
    assert!(
        !req.is_static,
        "Static follows the negotiation, not the query"
    );
    assert_eq!(req.output_container.as_deref(), Some("mp3"));
    assert_eq!(req.audio_codec.as_deref(), Some("mp3"));
    assert_eq!(req.audio_bitrate, Some(128_000));
    assert_eq!(req.max_audio_channels, Some(2));
    assert_eq!(req.audio_channels, Some(2));
    assert_eq!(req.audio_sample_rate, Some(48_000));
    assert_eq!(req.max_audio_bit_depth, Some(16));
    assert_eq!(req.start_time_ticks, Some(50_000_000));
    assert_eq!(req.device_id.as_deref(), Some("dev1"));
    assert!(
        req.enable_audio_vbr_encoding,
        "`enableAudioVbrEncoding` defaults to true"
    );
    assert_eq!(
        req.context,
        Some(ferrofin_model::dlna::EncodingContext::Static)
    );
    assert!(
        req.play_session_id
            .as_deref()
            .is_some_and(|p| p.len() == 32)
    );
    // The trusted policy rides the request, as on every planning route.
    assert_eq!(
        req.playback_permissions,
        Some(ferrofin_traits::library::PlaybackPermissions {
            video_transcoding: false,
            audio_transcoding: true,
            remuxing: false,
        })
    );
    assert!(
        req.transcode_reasons
            .as_deref()
            .is_some_and(|r| r.contains("ContainerNotSupported")),
        "{:?}",
        req.transcode_reasons
    );
}

/// `enableAudioVbrEncoding=false` rides the universal request into the
/// transcode, as `UniversalAudioController` passes it into its DTOs.
#[tokio::test]
async fn a_refused_vbr_encoding_reaches_the_transcode() {
    let file = temp_file();
    let (app, served) = router(vec![flac_source(ITEM_ID, &file.path, 900_000)], &file.path);
    let resp = get(
        app,
        "container=mp3&transcodingContainer=mp3&audioCodec=mp3&maxStreamingBitrate=128000\
         &enableAudioVbrEncoding=false",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!progressive(&served).enable_audio_vbr_encoding);
}

/// A listed container whose bitrate exceeds the client's cap is transcoded.
#[tokio::test]
async fn a_bitrate_cap_below_the_file_forces_a_transcode() {
    let file = temp_file();
    let (app, served) = router(vec![flac_source(ITEM_ID, &file.path, 900_000)], &file.path);
    let resp = get(
        app,
        "container=flac,mp3&transcodingContainer=mp3&audioCodec=mp3&maxStreamingBitrate=128000",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let req = progressive(&served);
    assert_eq!(req.audio_bitrate, Some(128_000));
    assert!(
        req.transcode_reasons
            .as_deref()
            .is_some_and(|r| r.contains("ContainerBitrateExceedsLimit")),
        "{:?}",
        req.transcode_reasons
    );
}

/// No container list (upstream's empty `DirectPlayProfiles`) is transcoded,
/// and with no container or codec named either the planner picks a matching
/// pair rather than a codec the container cannot hold.
#[tokio::test]
async fn no_container_list_is_transcoded() {
    let file = temp_file();
    let (app, served) = router(vec![flac_source(ITEM_ID, &file.path, 900_000)], &file.path);
    let resp = get(app, "").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let req = progressive(&served);
    assert_eq!(req.output_container, None);
    assert_eq!(req.audio_codec, None);
}

/// `transcodingProtocol=hls` answers with the audio master playlist and
/// upstream's `HlsAudioRequestDto`.
#[tokio::test]
async fn the_hls_protocol_returns_an_audio_master_playlist() {
    let file = temp_file();
    let (app, served) = router(vec![flac_source(ITEM_ID, &file.path, 900_000)], &file.path);
    let resp = get(
        app,
        "container=mp3,aac&transcodingContainer=ts&transcodingProtocol=hls&audioCodec=aac\
         &maxStreamingBitrate=192000&audioBitRate=160000&maxAudioSampleRate=48000",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "application/vnd.apple.mpegurl"
    );
    let req = master(&served);
    assert_eq!(req.segment_container.as_deref(), Some("ts"));
    assert_eq!(req.audio_codec.as_deref(), Some("aac"));
    assert_eq!(req.audio_bitrate, Some(160_000));
    assert_eq!(req.audio_sample_rate, Some(48_000));
    assert!(!req.is_static);
    assert!(!req.enable_adaptive_bitrate_streaming);
    assert!(req.query_string.starts_with("?container=mp3,aac"));
}

/// An HLS segment container outside {ts, mp4} falls back to `ts`.
#[tokio::test]
async fn an_unsupported_hls_segment_container_falls_back_to_ts() {
    let file = temp_file();
    let (app, served) = router(vec![flac_source(ITEM_ID, &file.path, 900_000)], &file.path);
    let resp = get(
        app,
        "container=mp3&transcodingContainer=aac&transcodingProtocol=hls&audioCodec=aac",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(master(&served).segment_container.as_deref(), Some("ts"));
}

/// A remote direct-playable source is redirected to when the client allows it.
#[tokio::test]
async fn a_remote_source_redirects_when_remote_media_is_enabled() {
    let file = temp_file();
    let mut remote = flac_source(ITEM_ID, "https://media.example.invalid/a.flac", 900_000);
    remote.protocol = MediaProtocol::Http;
    remote.is_remote = true;
    let (app, served) = router(vec![remote], &file.path);
    let resp = get(app, "container=flac&enableRemoteMedia=true").await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        resp.headers()[header::LOCATION],
        "https://media.example.invalid/a.flac"
    );
    assert!(served.lock().unwrap().is_none());

    // Without `enableRemoteMedia=true`, or with `enableRedirection=false`, the
    // source is served by the server instead (relayed; this origin is unreachable).
    for query in [
        "container=flac",
        "container=flac&enableRemoteMedia=true&enableRedirection=false",
    ] {
        let mut remote = flac_source(ITEM_ID, "https://media.example.invalid/a.flac", 900_000);
        remote.protocol = MediaProtocol::Http;
        remote.is_remote = true;
        let (app, _) = router(vec![remote], &file.path);
        let resp = get(app, query).await;
        assert_ne!(resp.status(), StatusCode::FOUND, "{query}");
        assert!(resp.headers().get(header::LOCATION).is_none(), "{query}");
    }
}

/// `HEAD` answers each branch with its headers and no body: the original file
/// with its length and `Accept-Ranges`, the master playlist with its MIME type.
#[tokio::test]
async fn head_answers_the_static_and_hls_branches_without_a_body() {
    let file = temp_file();
    let (app, _) = router(vec![flac_source(ITEM_ID, &file.path, 900_000)], &file.path);
    let resp = send(app, "HEAD", "container=flac").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(resp.headers()[header::CONTENT_LENGTH], "10");
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body.is_empty());

    let (app, served) = router(vec![flac_source(ITEM_ID, &file.path, 900_000)], &file.path);
    let resp = send(
        app,
        "HEAD",
        "container=mp3&transcodingContainer=ts&transcodingProtocol=hls&audioCodec=aac",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "application/vnd.apple.mpegurl"
    );
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body.is_empty());
    master(&served);
}

/// An invalid codec or container identifier is refused before negotiation, as
/// upstream's `[RegularExpression]` validation does.
#[tokio::test]
async fn an_invalid_codec_identifier_is_a_bad_request() {
    let file = temp_file();
    let (app, _served) = router(vec![flac_source(ITEM_ID, &file.path, 900_000)], &file.path);
    let resp = get(app, "container=mp3&audioCodec=mp3;rm").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// The protocol binds ignoring case, and a value that is no member leaves it
/// unset (upstream's `NullableEnumModelBinder`), so the transcode is HTTP.
#[tokio::test]
async fn the_transcoding_protocol_binds_like_a_nullable_enum() {
    let file = temp_file();
    let (app, served) = router(vec![flac_source(ITEM_ID, &file.path, 900_000)], &file.path);
    let resp = get(
        app,
        "container=mp3&transcodingContainer=ts&transcodingProtocol=HLS",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    master(&served);

    let (app, served) = router(vec![flac_source(ITEM_ID, &file.path, 900_000)], &file.path);
    let resp = get(
        app,
        "container=mp3&transcodingContainer=mp3&transcodingProtocol=dash",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    progressive(&served);
}

/// When only the container is unsupported, the HLS branch copies the audio
/// (`AudioCodec = "copy"`) instead of re-encoding it.
#[tokio::test]
async fn an_hls_container_mismatch_alone_copies_the_audio() {
    let file = temp_file();
    let mut aac = flac_source(ITEM_ID, &file.path, 256_000);
    aac.container = Some("m4a".to_owned());
    aac.media_streams[0].codec = Some("aac".to_owned());
    let (app, served) = router(vec![aac], &file.path);
    let resp = get(
        app,
        "container=mp3,ts|aac&transcodingContainer=ts&transcodingProtocol=hls&audioCodec=aac",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let req = master(&served);
    assert_eq!(
        req.transcode_reasons.as_deref(),
        Some("ContainerNotSupported")
    );
    assert_eq!(req.audio_codec.as_deref(), Some("copy"));
}

/// A user without audio-transcoding permission is served the original file, as
/// upstream: the source cannot transcode, so no stream is negotiated and its
/// direct-stream flag stands.
#[tokio::test]
async fn a_user_who_may_not_transcode_is_served_the_original_file() {
    let file = temp_file();
    let (app, served) = router_as(
        vec![flac_source(ITEM_ID, &file.path, 900_000)],
        &file.path,
        false,
    );
    let resp = get(app, "container=mp3&transcodingContainer=mp3&audioCodec=mp3").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::ACCEPT_RANGES], "bytes");
    assert!(served.lock().unwrap().is_none());
}

/// `mediaSourceId` narrows the negotiation to that version, and a version the
/// item does not have is a `404`.
#[tokio::test]
async fn the_media_source_id_picks_the_version() {
    let file = temp_file();
    let other = Uuid::from_u128(0x00C1_0002);
    let sources = || {
        let mut mp3 = flac_source(other, &file.path, 128_000);
        mp3.container = Some("mp3".to_owned());
        mp3.media_streams[0].codec = Some("mp3".to_owned());
        vec![flac_source(ITEM_ID, &file.path, 900_000), mp3]
    };
    // Unnamed, the item's own (FLAC) version is negotiated and transcoded.
    let (app, served) = router(sources(), &file.path);
    let resp = get(app, "container=mp3&transcodingContainer=mp3").await;
    assert_eq!(resp.status(), StatusCode::OK);
    progressive(&served);

    // Named, the mp3 version plays as it is.
    let (app, served) = router(sources(), &file.path);
    let resp = get(
        app,
        &format!("container=mp3&mediaSourceId={}", other.simple()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::ACCEPT_RANGES], "bytes");
    assert!(served.lock().unwrap().is_none());

    let (app, _) = router(sources(), &file.path);
    let resp = get(
        app,
        "container=mp3&mediaSourceId=ffffffffffffffffffffffffffffffff",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
