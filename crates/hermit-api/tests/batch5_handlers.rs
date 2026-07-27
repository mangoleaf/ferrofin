//! Batch-5 handler tests: on-the-fly subtitle transcode + FallbackFont.
//!
//! Drives the real handlers through `tower::ServiceExt::oneshot`:
//! - `GET /Videos/{id}/{source}/Subtitles/{index}/{format}` (+ the ticks and
//!   `subtitles.m3u8` variants) call the [`SubtitleEncoder`] seam;
//! - `GET /FallbackFont/Fonts` + `/{name}` resolve `EncodingOptions.
//!   FallbackFontPath` via the config seam and read fonts through the
//!   [`FileSystem`] seam.
//!
//! Purpose-built fakes stand in for the four managers these handlers touch
//! (library, config, file-system, media-sources) plus the encoder; every other
//! manager is a `test_support` panic double, catching a strayed handler.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{TimeZone, Utc};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{authed_state_for_subtitles, minimal_base_item};
use hermit_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use hermit_model::configuration::EncodingOptions;
use hermit_model::data::CollectionType;
use hermit_model::dto::{ItemCounts, MediaSourceInfo};
use hermit_model::entities::MediaStreamType;
use hermit_model::entities_media::{MediaAttachment, MediaStream};
use hermit_model::media_info::LiveStreamRequest;
use hermit_model::querying::{QueryFiltersLegacy, QueryResult};
use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::error::ServiceError;
use hermit_traits::filesystem::{FileMetadata, FileSystem};
use hermit_traits::library::{LibraryManager, MediaSourceManager};
use hermit_traits::media_encoding::SubtitleEncoder;
use hermit_traits::options::{DeleteOptions, InternalItemsQuery, InternalPeopleQuery};
use hermit_traits::persistence::ItemWithCounts;
use hermit_traits::system::ServerApplicationPaths;
use tower::ServiceExt;
use uuid::Uuid;

const ITEM_ID: Uuid = Uuid::from_u128(0x00B5_0001);

// ---- Fakes -----------------------------------------------------------------

/// A library where only [`ITEM_ID`] exists.
struct OneItemLibrary;

#[async_trait]
impl LibraryManager for OneItemLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((id == ITEM_ID).then(|| minimal_base_item(id, "The Item", "Movie")))
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
        unimplemented!("unused")
    }
}

/// A config manager returning a fixed [`EncodingOptions`].
struct FontConfig(EncodingOptions);

#[async_trait]
impl ServerConfigurationManager for FontConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        unimplemented!("unused")
    }
    async fn configuration(
        &self,
    ) -> Result<hermit_model::configuration::ServerConfiguration, ServiceError> {
        unimplemented!("unused")
    }
    async fn update_configuration(
        &self,
        _configuration: &hermit_model::configuration::ServerConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn get_branding(&self) -> Result<hermit_model::branding::BrandingOptions, ServiceError> {
        unimplemented!("unused")
    }
    async fn update_branding(
        &self,
        _branding: &hermit_model::branding::BrandingOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn get_encoding_options(&self) -> Result<EncodingOptions, ServiceError> {
        Ok(self.0.clone())
    }
}

/// A file-system serving a fixed set of font files (and their bytes).
struct FontFs {
    files: Vec<FileMetadata>,
    bytes: Vec<u8>,
}

impl FileSystem for FontFs {
    fn get_file_system_entries(&self, _path: &str) -> Vec<hermit_model::io::FileSystemEntryInfo> {
        unimplemented!("unused")
    }
    fn get_drives(&self) -> Vec<hermit_model::io::FileSystemEntryInfo> {
        unimplemented!("unused")
    }
    fn file_exists(&self, _path: &str) -> bool {
        unimplemented!("unused")
    }
    fn directory_exists(&self, _path: &str) -> bool {
        unimplemented!("unused")
    }
    fn validate_writable(&self, _path: &str) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    fn get_files(&self, _path: &str, _extensions: &[&str]) -> Vec<FileMetadata> {
        self.files.clone()
    }
    fn read_file(&self, _path: &str) -> Result<Vec<u8>, ServiceError> {
        Ok(self.bytes.clone())
    }
}

/// A media-source manager returning a fixed playback source list.
struct FixedSources(Vec<MediaSourceInfo>);

#[async_trait]
impl MediaSourceManager for FixedSources {
    async fn get_media_streams(&self, _item_id: Uuid) -> Result<Vec<MediaStream>, ServiceError> {
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
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        Ok(self.0.clone())
    }
    async fn get_static_media_sources(
        &self,
        _item_id: Uuid,
        _enable_path_substitution: bool,
        _user_id: Option<Uuid>,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        unimplemented!("unused")
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
    async fn refresh_media_streams(&self, _item_id: uuid::Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
}

/// A subtitle encoder returning fixed bytes (or an error).
struct StubEncoder(Result<Vec<u8>, ()>);

#[async_trait]
impl SubtitleEncoder for StubEncoder {
    async fn get_subtitles(
        &self,
        _item_id: Uuid,
        _media_source_id: &str,
        _subtitle_stream_index: i32,
        _output_format: &str,
        _start_time_ticks: i64,
        _end_time_ticks: i64,
        _preserve_original_timestamps: bool,
    ) -> Result<Vec<u8>, ServiceError> {
        self.0
            .clone()
            .map_err(|()| ServiceError::not_found("no subtitle"))
    }
    async fn get_subtitle_file_character_set(
        &self,
        _subtitle_stream: &MediaStream,
        _language: &str,
        _media_source: &MediaSourceInfo,
    ) -> Result<String, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_subtitle_file_path(
        &self,
        _subtitle_stream: &MediaStream,
        _media_source: &MediaSourceInfo,
    ) -> Result<String, ServiceError> {
        unimplemented!("unused")
    }
    async fn extract_all_extractable_subtitles(
        &self,
        _media_source: &MediaSourceInfo,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
}

// ---- Helpers ---------------------------------------------------------------

fn font(name: &str, size: i64) -> FileMetadata {
    let ts = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    FileMetadata {
        name: name.to_owned(),
        full_name: format!("/fonts/{name}"),
        length: size,
        date_created: ts,
        date_modified: ts,
    }
}

fn subtitle_state(encoder: StubEncoder) -> AppState {
    authed_state_for_subtitles(
        Arc::new(OneItemLibrary),
        Arc::new(FontConfig(EncodingOptions::default())),
        Arc::new(FontFs {
            files: Vec::new(),
            bytes: Vec::new(),
        }),
        Arc::new(FixedSources(Vec::new())),
        Arc::new(encoder),
    )
}

fn font_state(options: EncodingOptions, fs: FontFs) -> AppState {
    authed_state_for_subtitles(
        Arc::new(OneItemLibrary),
        Arc::new(FontConfig(options)),
        Arc::new(fs),
        Arc::new(FixedSources(Vec::new())),
        Arc::new(StubEncoder(Ok(Vec::new()))),
    )
}

fn playlist_state(sources: Vec<MediaSourceInfo>) -> AppState {
    authed_state_for_subtitles(
        Arc::new(OneItemLibrary),
        Arc::new(FontConfig(EncodingOptions::default())),
        Arc::new(FontFs {
            files: Vec::new(),
            bytes: Vec::new(),
        }),
        Arc::new(FixedSources(sources)),
        Arc::new(StubEncoder(Ok(Vec::new()))),
    )
}

async fn call(app: AppState, uri: &str) -> (StatusCode, Vec<u8>, Option<String>) {
    let router = create_router(app);
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("x-emby-token", "tok")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body, content_type)
}

// ---- On-the-fly subtitle conversion ----------------------------------------

#[tokio::test]
async fn get_subtitle_converts_and_serves_vtt() {
    let app = subtitle_state(StubEncoder(Ok(b"WEBVTT\n\nHi".to_vec())));
    let (status, body, ct) = call(
        app,
        &format!("/Videos/{ITEM_ID}/msrc/Subtitles/0/Stream.vtt"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct.as_deref(), Some("text/vtt"));
    assert_eq!(body, b"WEBVTT\n\nHi");
}

#[tokio::test]
async fn get_subtitle_adds_vtt_time_map_when_requested() {
    let app = subtitle_state(StubEncoder(Ok(b"WEBVTT\n\nHi".to_vec())));
    let (status, body, _) = call(
        app,
        &format!("/Videos/{ITEM_ID}/msrc/Subtitles/0/Stream.vtt?AddVttTimeMap=true"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("X-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000"));
}

#[tokio::test]
async fn get_subtitle_js_alias_maps_to_json_mime() {
    let app = subtitle_state(StubEncoder(Ok(b"[]".to_vec())));
    let (status, _, ct) = call(
        app,
        &format!("/Videos/{ITEM_ID}/msrc/Subtitles/0/Stream.js"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // js → json; JSON subtitles are served as text/plain by the small MIME map.
    assert_eq!(ct.as_deref(), Some("text/plain"));
}

#[tokio::test]
async fn get_subtitle_with_ticks_route_reaches_encoder() {
    let app = subtitle_state(StubEncoder(Ok(b"WEBVTT".to_vec())));
    let (status, body, _) = call(
        app,
        &format!("/Videos/{ITEM_ID}/msrc/Subtitles/0/1230000/Stream.vtt"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"WEBVTT");
}

#[tokio::test]
async fn get_subtitle_disabled_encoder_is_not_found() {
    let app = subtitle_state(StubEncoder(Err(())));
    let (status, _, _) = call(
        app,
        &format!("/Videos/{ITEM_ID}/msrc/Subtitles/0/Stream.vtt"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---- HLS subtitle playlist -------------------------------------------------

fn source_with_runtime(id: &str, runtime: Option<i64>) -> MediaSourceInfo {
    MediaSourceInfo {
        id: Some(id.to_owned()),
        run_time_ticks: runtime,
        ..MediaSourceInfo::default()
    }
}

#[tokio::test]
async fn subtitle_playlist_builds_segments() {
    // 25s runtime, 10s segments → 3 segments.
    let app = playlist_state(vec![source_with_runtime("msrc", Some(250_000_000))]);
    let (status, body, ct) = call(
        app,
        &format!("/Videos/{ITEM_ID}/msrc/Subtitles/0/subtitles.m3u8?segmentLength=10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct.as_deref(), Some("application/x-mpegURL"));
    let text = String::from_utf8(body).unwrap();
    assert!(text.starts_with("#EXTM3U"));
    assert!(text.contains("#EXT-X-TARGETDURATION:10"));
    assert!(text.trim_end().ends_with("#EXT-X-ENDLIST"));
    assert_eq!(text.matches("stream.vtt?").count(), 3);
}

#[tokio::test]
async fn subtitle_playlist_missing_item_is_not_found() {
    let app = playlist_state(vec![source_with_runtime("msrc", Some(250_000_000))]);
    let other = Uuid::from_u128(0xDEAD);
    let (status, _, _) = call(
        app,
        &format!("/Videos/{other}/msrc/Subtitles/0/subtitles.m3u8?segmentLength=10"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn subtitle_playlist_zero_runtime_is_bad_request() {
    let app = playlist_state(vec![source_with_runtime("msrc", Some(0))]);
    let (status, _, _) = call(
        app,
        &format!("/Videos/{ITEM_ID}/msrc/Subtitles/0/subtitles.m3u8?segmentLength=10"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn subtitle_playlist_zero_segment_length_is_bad_request() {
    let app = playlist_state(vec![source_with_runtime("msrc", Some(250_000_000))]);
    let (status, _, _) = call(
        app,
        &format!("/Videos/{ITEM_ID}/msrc/Subtitles/0/subtitles.m3u8?segmentLength=0"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn subtitle_playlist_unknown_source_is_not_found() {
    let app = playlist_state(vec![source_with_runtime("other", Some(250_000_000))]);
    let (status, _, _) = call(
        app,
        &format!("/Videos/{ITEM_ID}/msrc/Subtitles/0/subtitles.m3u8?segmentLength=10"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---- FallbackFont ----------------------------------------------------------

fn options_with_path(path: Option<&str>) -> EncodingOptions {
    EncodingOptions {
        fallback_font_path: path.map(str::to_owned),
        ..EncodingOptions::default()
    }
}

#[tokio::test]
async fn fallback_font_list_empty_when_unconfigured() {
    let app = font_state(
        options_with_path(None),
        FontFs {
            files: Vec::new(),
            bytes: Vec::new(),
        },
    );
    let (status, body, _) = call(app, "/FallbackFont/Fonts").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"[]");
}

#[tokio::test]
async fn fallback_font_list_returns_sorted_fonts() {
    let app = font_state(
        options_with_path(Some("/fonts")),
        FontFs {
            files: vec![font("b.ttf", 200), font("a.ttf", 100)],
            bytes: Vec::new(),
        },
    );
    let (status, body, _) = call(app, "/FallbackFont/Fonts").await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    // Sorted by size ascending → a.ttf (100) precedes b.ttf (200).
    let a = text.find("a.ttf").unwrap();
    let b = text.find("b.ttf").unwrap();
    assert!(a < b, "fonts should be size-ordered: {text}");
}

#[tokio::test]
async fn fallback_font_list_stops_at_size_cap() {
    // Two 15 MiB fonts: the running total reaches the 20 MiB cap on the second,
    // so only the first is served.
    let fifteen_mib = 15 * 1024 * 1024;
    let app = font_state(
        options_with_path(Some("/fonts")),
        FontFs {
            files: vec![font("a.ttf", fifteen_mib), font("b.ttf", fifteen_mib)],
            bytes: Vec::new(),
        },
    );
    let (status, body, _) = call(app, "/FallbackFont/Fonts").await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("a.ttf"));
    assert!(!text.contains("b.ttf"), "second font past cap: {text}");
}

#[tokio::test]
async fn fallback_font_serves_named_file() {
    let app = font_state(
        options_with_path(Some("/fonts")),
        FontFs {
            files: vec![font("Roboto.ttf", 42)],
            bytes: b"FONTBYTES".to_vec(),
        },
    );
    let (status, body, ct) = call(app, "/FallbackFont/Fonts/roboto.ttf").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct.as_deref(), Some("font/ttf"));
    assert_eq!(body, b"FONTBYTES");
}

#[tokio::test]
async fn fallback_font_unconfigured_path_is_ok_empty() {
    let app = font_state(
        options_with_path(None),
        FontFs {
            files: Vec::new(),
            bytes: Vec::new(),
        },
    );
    let (status, body, _) = call(app, "/FallbackFont/Fonts/roboto.ttf").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}

#[tokio::test]
async fn fallback_font_missing_named_file_is_ok_empty() {
    let app = font_state(
        options_with_path(Some("/fonts")),
        FontFs {
            files: vec![font("Roboto.ttf", 42)],
            bytes: b"FONTBYTES".to_vec(),
        },
    );
    let (status, body, _) = call(app, "/FallbackFont/Fonts/absent.ttf").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}
