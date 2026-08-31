//! `SubtitleController` — subtitle management + fallback fonts.
//!
//! Ports the portable slice of Jellyfin's `SubtitleController`:
//! - `DELETE /Videos/{itemId}/Subtitles/{index}` — delete a stored external
//!   subtitle stream (+ its sidecar file).
//! - `POST /Videos/{itemId}/Subtitles` — upload an external subtitle file.
//! - `GET /Items/{itemId}/RemoteSearch/Subtitles/{language}` — search providers.
//! - `POST /Items/{itemId}/RemoteSearch/Subtitles/{subtitleId}` — download one.
//! - `GET /Providers/Subtitles/Subtitles/{subtitleId}` — fetch a remote subtitle.
//!
//! The delete route is DB-backed and real. Upload / remote search / download /
//! get drive the un-ported `ISubtitleProvider` registry (deferred); the routes
//! exist (not `501`) and surface the manager's empty/"not enabled" behaviour so
//! clients see stable semantics.
//!
//! On-the-fly subtitle *conversion* is now real: the
//! `Videos/{itemId}/{container}/Subtitles/{index}/{format}` (with and without a
//! start-position segment) routes call the
//! [`SubtitleEncoder`](ferrofin_traits::media_encoding::SubtitleEncoder) seam
//! (ffmpeg-backed `SubtitleEncoderImpl` at the composition root; the disabled
//! stub `404`s), and the `subtitles.m3u8` route builds the segment playlist from
//! the media source's runtime. The `FallbackFont` routes resolve
//! `EncodingOptions.FallbackFontPath` (via the config seam's new
//! `get_encoding_options`) and enumerate / serve fonts through the
//! [`FileSystem`](ferrofin_traits::filesystem::FileSystem) seam.

use std::fmt::Write as _;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use ferrofin_model::providers::RemoteSubtitleInfo;
use ferrofin_model::secret::Secret;
use ferrofin_model::subtitles::FontFile;
use ferrofin_traits::subtitles::{SubtitleResponse, SubtitleSearchRequest};
use uuid::Uuid;

use crate::auth::{RequireAdmin, RequireAuth};
use crate::error::ApiError;
use crate::extract::JsonBody;
use crate::handlers::image_upload::decode_base64;
use crate::handlers::items::resolve_user_opt;
use crate::handlers::queue_high_priority_refresh;
use crate::state::AppState;

/// The MIME type Jellyfin serves an HLS subtitle playlist with
/// (`MimeTypes.GetMimeType("playlist.m3u8")`).
const HLS_PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";

/// The number of 100-ns ticks in one second (`TimeSpan.TicksPerSecond`), used to
/// convert the `segmentLength` (seconds) into the tick space the playlist math
/// and the subtitle encoder operate in.
const TICKS_PER_SECOND: i64 = 10_000_000;

/// The maximum total size of fallback fonts served by `GET /FallbackFont/Fonts`
/// (Jellyfin's hard-coded 20 MiB cap; fonts past it are dropped).
const FALLBACK_FONT_MAX_TOTAL_BYTES: i64 = 20 * 1024 * 1024;

/// The fallback-font file extensions Jellyfin enumerates
/// (`.woff`/`.woff2`/`.ttf`/`.otf`).
const FALLBACK_FONT_EXTENSIONS: &[&str] = &[".woff", ".woff2", ".ttf", ".otf"];

/// Ensures an item exists, returning `404` otherwise.
async fn require_item(state: &AppState, item_id: Uuid) -> Result<(), ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }
    Ok(())
}

/// `DELETE /Videos/{itemId}/Subtitles/{index}` — delete a stored subtitle.
///
/// Port of `SubtitleController.DeleteSubtitle`: `404` when the item is missing,
/// else `204`. The manager drops the external subtitle stream at `index` and its
/// sidecar file (deleting a non-existent index is idempotent). Elevation policy
/// is deferred to the auth layer.
#[utoipa::path(
    delete,
    path = "/Videos/{itemId}/Subtitles/{index}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("index" = i32, Path, description = "The index of the subtitle file")
    ),
    responses(
        (status = 204, description = "Subtitle deleted"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn delete_subtitle(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path((item_id, index)): Path<(Uuid, i32)>,
) -> Result<StatusCode, ApiError> {
    require_item(&state, item_id).await?;
    state.subtitles.delete_subtitles(item_id, index).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The `POST /Videos/{itemId}/Subtitles` request body — a base64-encoded subtitle
/// plus its metadata (port of the C# `UploadSubtitleDto`).
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "PascalCase")]
struct UploadSubtitleDto {
    /// The subtitle language (three-letter ISO code).
    language: String,
    /// The subtitle format (e.g. `srt`, `ass`).
    format: String,
    /// Whether the subtitle is forced.
    is_forced: bool,
    /// Whether the subtitle is for the hearing impaired (SDH).
    is_hearing_impaired: bool,
    /// The base64-encoded subtitle file bytes.
    data: String,
}

/// `POST /Videos/{itemId}/Subtitles` — upload an external subtitle file.
///
/// Port of `SubtitleController.UploadSubtitle`: `404` when the item is missing,
/// `400` when the `Data` is not valid base64. The decoded bytes are handed to the
/// [`SubtitleManager`](ferrofin_traits::subtitles::SubtitleManager); with no
/// subtitle-provider host wired the manager rejects the write (`400`), otherwise
/// a metadata refresh is queued and `204` returned. Elevation policy is deferred.
#[utoipa::path(
    post,
    path = "/Videos/{itemId}/Subtitles",
    params(("itemId" = String, Path, description = "The item the subtitle belongs to")),
    request_body = UploadSubtitleDto,
    responses(
        (status = 204, description = "Subtitle uploaded"),
        (status = 400, description = "Invalid subtitle data"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn upload_subtitle(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    JsonBody(body): JsonBody<UploadSubtitleDto>,
) -> Result<StatusCode, ApiError> {
    require_item(&state, item_id).await?;
    let content = decode_base64(&body.data)
        .ok_or_else(|| ApiError::BadRequest("subtitle data is not valid base64".to_owned()))?;
    let response = SubtitleResponse {
        language: body.language,
        format: body.format,
        is_forced: body.is_forced,
        is_hearing_impaired: body.is_hearing_impaired,
        content,
    };
    state.subtitles.upload_subtitle(item_id, &response).await?;
    queue_high_priority_refresh(&state, item_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for `GET /Items/{itemId}/RemoteSearch/Subtitles/{language}`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteSearchQuery {
    /// Optional. Only show subtitles which are a perfect match.
    #[serde(default)]
    is_perfect_match: Option<bool>,
}

/// `GET /Items/{itemId}/RemoteSearch/Subtitles/{language}` — search providers.
///
/// Port of `SubtitleController.SearchRemoteSubtitles`: `404` for a missing item,
/// else the (possibly empty) provider results for the language.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/RemoteSearch/Subtitles/{language}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("language" = String, Path, description = "The language of the subtitles"),
        ("isPerfectMatch" = Option<bool>, Query, description = "Only show subtitles which are a perfect match")
    ),
    responses(
        (status = 200, description = "Subtitles retrieved", body = [RemoteSubtitleInfo]),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn search_remote_subtitles(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, language)): Path<(Uuid, String)>,
    Query(query): Query<RemoteSearchQuery>,
) -> Result<Json<Vec<RemoteSubtitleInfo>>, ApiError> {
    require_item(&state, item_id).await?;
    // The manager enriches this from the resolved item (name/year/imdb/…) before
    // querying providers; the handler supplies the caller-visible fields.
    let request = SubtitleSearchRequest {
        item_id,
        language,
        is_perfect_match: query.is_perfect_match,
        is_automated: false,
        ..Default::default()
    };
    let results = state.subtitles.search_subtitles(&request).await?;
    Ok(Json(results))
}

/// `POST /Items/{itemId}/RemoteSearch/Subtitles/{subtitleId}` — download one.
///
/// Port of `SubtitleController.DownloadRemoteSubtitles`: `404` for a missing
/// item, else `204` after attempting the download (the C# swallows download
/// errors and still returns `204`; a metadata refresh is queued). After router
/// normalization the trailing id segment is captured as `{language}`.
#[utoipa::path(
    post,
    path = "/Items/{itemId}/RemoteSearch/Subtitles/{language}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("language" = String, Path, description = "The subtitle id")
    ),
    responses(
        (status = 204, description = "Subtitle downloaded"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn download_remote_subtitles(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, subtitle_id)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    require_item(&state, item_id).await?;
    // The C# logs-and-continues on a provider failure, still returning 204, then
    // queues a refresh. A download that succeeds queues the refresh too.
    if state
        .subtitles
        .download_subtitles(item_id, &subtitle_id)
        .await
        .is_ok()
    {
        queue_high_priority_refresh(&state, item_id).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Providers/Subtitles/Subtitles/{subtitleId}` — fetch a remote subtitle.
///
/// Port of `SubtitleController.GetRemoteSubtitles`: streams the raw subtitle
/// bytes with a MIME type derived from its format. With no provider host wired
/// the fetch is rejected (`400`); a provider would yield the file.
#[utoipa::path(
    get,
    path = "/Providers/Subtitles/Subtitles/{subtitleId}",
    params(("subtitleId" = String, Path, description = "The subtitle id")),
    responses((status = 200, description = "File returned")),
    tag = "ferrofin"
)]
async fn get_remote_subtitles(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(subtitle_id): Path<String>,
) -> Result<Response, ApiError> {
    let result = state.subtitles.get_remote_subtitles(&subtitle_id).await?;
    let mime = subtitle_mime(&result.format);
    Ok(([(header::CONTENT_TYPE, mime)], result.content).into_response())
}

/// The MIME type for a subtitle of the given format (a small, common-format map;
/// unknown formats fall back to `text/plain`).
fn subtitle_mime(format: &str) -> &'static str {
    match format.to_ascii_lowercase().as_str() {
        "vtt" => "text/vtt",
        "srt" | "subrip" => "application/x-subrip",
        "ass" | "ssa" => "text/x-ssa",
        "json" => "application/json",
        _ => "text/plain",
    }
}

/// Extracts the subtitle output format from a captured route segment.
///
/// The axum route normalizes Jellyfin's `Stream.{routeFormat}` to a single
/// `{routeFormat}` capture holding the whole segment (e.g. `Stream.vtt`), so the
/// literal `Stream.` prefix is stripped and the trailing extension taken. A bare
/// segment without the prefix (defensive) is returned as-is. The C# `js` alias
/// maps to `json`.
fn parse_subtitle_format(segment: &str) -> String {
    let ext = segment
        .rsplit_once('.')
        .map_or(segment, |(_, ext)| ext)
        .to_ascii_lowercase();
    if ext == "js" { "json".to_owned() } else { ext }
}

/// Query parameters shared by the on-the-fly subtitle-conversion routes.
///
/// The `PascalCase` aliases match the query keys the server emits in its own HLS
/// subtitle-playlist links (`stream.vtt?CopyTimestamps=true&AddVttTimeMap=…`), so
/// those `stream.vtt` requests bind correctly regardless of the client's casing.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubtitleStreamQuery {
    /// Optional. The end position of the subtitle in ticks.
    #[serde(default, alias = "EndPositionTicks")]
    end_position_ticks: Option<i64>,
    /// Optional. Whether to copy (preserve) the original timestamps.
    #[serde(default, alias = "CopyTimestamps")]
    copy_timestamps: bool,
    /// Optional. Whether to prepend a WebVTT `X-TIMESTAMP-MAP` header.
    #[serde(default, alias = "AddVttTimeMap")]
    add_vtt_time_map: bool,
    /// The start position of the subtitle in ticks.
    #[serde(default, alias = "StartPositionTicks")]
    start_position_ticks: i64,
}

/// Serves an encoded subtitle: converts the stream, then wraps the bytes with the
/// format's MIME type, adding the WebVTT time-map header when requested.
async fn encode_subtitle_response(
    state: &AppState,
    item_id: Uuid,
    media_source_id: &str,
    index: i32,
    format: &str,
    query: &SubtitleStreamQuery,
) -> Result<Response, ApiError> {
    let bytes = state
        .subtitle_encoder
        .get_subtitles(
            item_id,
            media_source_id,
            index,
            format,
            query.start_position_ticks,
            query.end_position_ticks.unwrap_or(0),
            query.copy_timestamps,
        )
        .await?;

    let mime = subtitle_mime(format);

    // For WebVTT with AddVttTimeMap, splice the MPEG-TS offset the HLS spec wants
    // (port of the `WEBVTT` → `WEBVTT\nX-TIMESTAMP-MAP=…` string replace).
    if format.eq_ignore_ascii_case("vtt") && query.add_vtt_time_map {
        // Jellyfin reads the encoded stream through a BOM-detecting StreamReader and
        // re-encodes the text without a preamble, so the writer's BOM does not survive
        // this path.
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text).replace(
            "WEBVTT",
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000",
        );
        return Ok(([(header::CONTENT_TYPE, mime)], text.into_bytes()).into_response());
    }

    Ok(([(header::CONTENT_TYPE, mime)], bytes).into_response())
}

/// `GET /Videos/{itemId}/{container}/Subtitles/{index}/{routeFormat}` — convert a
/// subtitle stream to the requested format on the fly.
///
/// Port of `SubtitleController.GetSubtitle` (the `Stream.{format}` route). The
/// `{container}` segment is the media source id; `{routeFormat}` collapses the
/// `Stream.{format}` literal. The [`SubtitleEncoder`] resolves the source,
/// charset-normalizes the stream and converts it over `[start, end]`; with no
/// encoder wired (disabled stub) this surfaces as `404`.
#[utoipa::path(
    get,
    path = "/Videos/{itemId}/{container}/Subtitles/{index}/{routeFormat}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("container" = String, Path, description = "The media source id"),
        ("index" = i32, Path, description = "The subtitle stream index"),
        ("routeFormat" = String, Path, description = "The requested subtitle format (Stream.{format})"),
        ("endPositionTicks" = Option<i64>, Query, description = "The end position of the subtitle in ticks"),
        ("copyTimestamps" = Option<bool>, Query, description = "Whether to copy the timestamps"),
        ("addVttTimeMap" = Option<bool>, Query, description = "Whether to add a VTT time map"),
        ("startPositionTicks" = Option<i64>, Query, description = "The start position of the subtitle in ticks")
    ),
    responses((status = 200, description = "File returned")),
    tag = "ferrofin"
)]
async fn get_subtitle(
    State(state): State<AppState>,
    Path((item_id, media_source_id, index, route_format)): Path<(Uuid, String, i32, String)>,
    Query(query): Query<SubtitleStreamQuery>,
) -> Result<Response, ApiError> {
    let format = parse_subtitle_format(&route_format);
    encode_subtitle_response(&state, item_id, &media_source_id, index, &format, &query).await
}

/// `GET /Videos/{itemId}/{container}/Subtitles/{index}/{routeFormat}/{routeFormat}`
/// — convert a subtitle stream, with the start position carried in the path.
///
/// Port of `SubtitleController.GetSubtitleWithTicks`: the first trailing segment
/// is the start-position ticks, the second is the `Stream.{format}` literal. The
/// path start position wins when the query omits it (matching the C# default).
#[utoipa::path(
    get,
    path = "/Videos/{itemId}/{container}/Subtitles/{index}/{routeStartPositionTicks}/{routeFormat}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("container" = String, Path, description = "The media source id"),
        ("index" = i32, Path, description = "The subtitle stream index"),
        ("routeStartPositionTicks" = i64, Path, description = "The start position of the subtitle in ticks"),
        ("routeFormat" = String, Path, description = "The requested subtitle format (Stream.{format})"),
        ("endPositionTicks" = Option<i64>, Query, description = "The end position of the subtitle in ticks"),
        ("copyTimestamps" = Option<bool>, Query, description = "Whether to copy the timestamps"),
        ("addVttTimeMap" = Option<bool>, Query, description = "Whether to add a VTT time map")
    ),
    responses((status = 200, description = "File returned")),
    tag = "ferrofin"
)]
async fn get_subtitle_with_ticks(
    State(state): State<AppState>,
    Path((item_id, media_source_id, index, start_position_ticks, route_format)): Path<(
        Uuid,
        String,
        i32,
        i64,
        String,
    )>,
    Query(mut query): Query<SubtitleStreamQuery>,
) -> Result<Response, ApiError> {
    // The route-supplied start position wins unless the query overrides it (the
    // C# `startPositionTicks ?? routeStartPositionTicks`); serde defaults the
    // query field to 0, so a 0 there yields the route value.
    if query.start_position_ticks == 0 {
        query.start_position_ticks = start_position_ticks;
    }
    let format = parse_subtitle_format(&route_format);
    encode_subtitle_response(&state, item_id, &media_source_id, index, &format, &query).await
}

/// Query parameters for `GET …/Subtitles/{index}/subtitles.m3u8`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubtitlePlaylistQuery {
    /// The subtitle segment length, in seconds.
    #[serde(default)]
    segment_length: i32,
}

/// `GET /Videos/{itemId}/{container}/Subtitles/{index}/subtitles.m3u8` — the HLS
/// subtitle playlist for a stream.
///
/// Port of `SubtitleController.GetSubtitlePlaylist`: `404` for a missing item,
/// `400` when the media source has no runtime or `segmentLength` is not positive.
/// Each segment is a relative `stream.vtt?…` link (with `CopyTimestamps` +
/// `AddVttTimeMap` + the position window + the caller's `ApiKey`), matching the
/// C# builder byte-for-byte.
#[utoipa::path(
    get,
    path = "/Videos/{itemId}/{container}/Subtitles/{index}/subtitles.m3u8",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("container" = String, Path, description = "The media source id"),
        ("index" = i32, Path, description = "The subtitle stream index"),
        ("segmentLength" = i32, Query, description = "The subtitle segment length")
    ),
    responses(
        (status = 200, description = "Subtitle playlist retrieved"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn get_subtitle_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    // The subtitle stream index is part of the URL but does not affect the
    // playlist body (the segments all point at the same `stream.vtt`); the C#
    // suppresses the same unused parameter (CA1801).
    Path((item_id, media_source_id, _index)): Path<(Uuid, String, i32)>,
    Query(query): Query<SubtitlePlaylistQuery>,
) -> Result<Response, ApiError> {
    require_item(&state, item_id).await?;

    let user = resolve_user_opt(&state, &auth, None).await?;
    let user_id = user
        .as_ref()
        .and_then(|u| Uuid::parse_str(&u.id).ok())
        .unwrap_or_else(Uuid::nil);

    let sources = state
        .media_sources
        .get_playback_media_sources(item_id, user_id, false, false)
        .await?;
    let media_source = sources
        .into_iter()
        .find(|s| s.id_matches(&media_source_id))
        .ok_or_else(|| ApiError::NotFound(format!("media source {media_source_id}")))?;

    let runtime = media_source.run_time_ticks.unwrap_or(-1);
    if runtime <= 0 {
        return Err(ApiError::BadRequest(
            "HLS Subtitles are not supported for this media.".to_owned(),
        ));
    }

    let segment_length_ticks = i64::from(query.segment_length) * TICKS_PER_SECOND;
    if segment_length_ticks <= 0 {
        return Err(ApiError::BadRequest(
            "segmentLength was not given, or it was given incorrectly. (It should be bigger than 0)"
                .to_owned(),
        ));
    }

    let playlist = build_subtitle_playlist(
        runtime,
        query.segment_length,
        segment_length_ticks,
        auth.token.as_ref().map_or("", Secret::expose),
    );
    Ok((
        [(header::CONTENT_TYPE, HLS_PLAYLIST_CONTENT_TYPE)],
        playlist,
    )
        .into_response())
}

/// Builds the `#EXTM3U` subtitle playlist body.
///
/// Split out so the tick arithmetic (segment count, per-segment `#EXTINF`
/// durations, and the relative `stream.vtt?…` links) is unit-testable without a
/// request. Mirrors the C# `StringBuilder` output exactly (including the trailing
/// `#EXT-X-ENDLIST`).
fn build_subtitle_playlist(
    runtime_ticks: i64,
    segment_length_seconds: i32,
    segment_length_ticks: i64,
    access_token: &str,
) -> String {
    let mut builder = String::new();
    builder.push_str("#EXTM3U\n");
    let _ = writeln!(builder, "#EXT-X-TARGETDURATION:{segment_length_seconds}");
    builder.push_str("#EXT-X-VERSION:3\n");
    builder.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    builder.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");

    let mut position_ticks: i64 = 0;
    while position_ticks < runtime_ticks {
        let remaining = runtime_ticks - position_ticks;
        let length_ticks = remaining.min(segment_length_ticks);
        // Ticks → seconds as a float, matching `TimeSpan.FromTicks(..).TotalSeconds`.
        #[allow(clippy::cast_precision_loss)]
        let length_seconds = length_ticks as f64 / TICKS_PER_SECOND as f64;
        let _ = writeln!(builder, "#EXTINF:{length_seconds},");

        let end_position_ticks = runtime_ticks.min(position_ticks + segment_length_ticks);
        let _ = writeln!(
            builder,
            "stream.vtt?CopyTimestamps=true&AddVttTimeMap=true&StartPositionTicks={position_ticks}&EndPositionTicks={end_position_ticks}&ApiKey={access_token}"
        );

        position_ticks += segment_length_ticks;
    }

    builder.push_str("#EXT-X-ENDLIST\n");
    builder
}

/// `GET /FallbackFont/Fonts` — list the available fallback font files.
///
/// Port of `SubtitleController.GetFallbackFontList`: when `FallbackFontPath` is
/// configured, enumerate the `.woff`/`.woff2`/`.ttf`/`.otf` files, order them
/// (size, then name), and yield until the running total would reach the 20 MiB
/// cap. An unset path yields an empty list (the C# logs a warning and returns
/// nothing).
#[utoipa::path(
    get,
    path = "/FallbackFont/Fonts",
    responses((status = 200, description = "Information retrieved", body = [FontFile])),
    tag = "ferrofin"
)]
async fn get_fallback_font_list(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<Vec<FontFile>>, ApiError> {
    let options = state.config.get_encoding_options().await?;
    let Some(path) = options.fallback_font_path.filter(|p| !p.is_empty()) else {
        return Ok(Json(Vec::new()));
    };

    let mut files = state.file_system.get_files(&path, FALLBACK_FONT_EXTENSIONS);
    // Order by size, then name (the C# `OrderBy(Size).ThenBy(Name)`); the later
    // date tiebreakers never change the size/name ordering for distinct files.
    files.sort_by(|a, b| a.length.cmp(&b.length).then_with(|| a.name.cmp(&b.name)));

    let mut fonts = Vec::new();
    let mut size_counter: i64 = 0;
    for file in files {
        size_counter += file.length;
        if size_counter >= FALLBACK_FONT_MAX_TOTAL_BYTES {
            break;
        }
        fonts.push(FontFile {
            name: Some(file.name),
            size: file.length,
            date_created: file.date_created,
            date_modified: file.date_modified,
        });
    }
    Ok(Json(fonts))
}

/// `GET /FallbackFont/Fonts/{name}` — serve a single fallback font file.
///
/// Port of `SubtitleController.GetFallbackFont`: locate the named file under
/// `FallbackFontPath` (case-insensitively) and stream it with a font MIME type.
/// A missing path / font returns `200` with an empty body (the C# returns `Ok()`
/// rather than `204`, which would break SubtitlesOctopus).
#[utoipa::path(
    get,
    path = "/FallbackFont/Fonts/{name}",
    params(("name" = String, Path, description = "The name of the fallback font file to get")),
    responses((status = 200, description = "Fallback font file retrieved")),
    tag = "ferrofin"
)]
async fn get_fallback_font(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let options = state.config.get_encoding_options().await?;
    let Some(path) = options.fallback_font_path.filter(|p| !p.is_empty()) else {
        return Ok(StatusCode::OK.into_response());
    };

    let file = state
        .file_system
        .get_files(&path, &[])
        .into_iter()
        .find(|f| f.name.eq_ignore_ascii_case(&name));

    match file {
        Some(f) if f.length > 0 => {
            let bytes = state.file_system.read_file(&f.full_name)?;
            let mime = font_mime(&f.name);
            Ok(([(header::CONTENT_TYPE, mime)], bytes).into_response())
        }
        // Null/empty font: the C# returns `Ok()` (200, empty) to avoid breaking
        // the SubtitlesOctopus renderer.
        _ => Ok(StatusCode::OK.into_response()),
    }
}

/// The MIME type for a font file, keyed on its extension (unknown → the generic
/// `font/sfnt`).
fn font_mime(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()) {
        Some(ext) if ext == "woff" => "font/woff",
        Some(ext) if ext == "woff2" => "font/woff2",
        Some(ext) if ext == "ttf" => "font/ttf",
        Some(ext) if ext == "otf" => "font/otf",
        _ => "font/sfnt",
    }
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Videos/{itemId}/Subtitles", post(upload_subtitle))
        .route(
            "/Videos/{itemId}/Subtitles/{index}",
            delete(delete_subtitle),
        )
        .route(
            "/Items/{itemId}/RemoteSearch/Subtitles/{language}",
            get(search_remote_subtitles).post(download_remote_subtitles),
        )
        .route(
            "/Providers/Subtitles/Subtitles/{subtitleId}",
            get(get_remote_subtitles),
        )
        .route(
            "/Videos/{itemId}/{container}/Subtitles/{index}/subtitles.m3u8",
            get(get_subtitle_playlist),
        )
        .route(
            "/Videos/{itemId}/{container}/Subtitles/{index}/{routeFormat}",
            get(get_subtitle),
        )
        .route(
            "/Videos/{itemId}/{container}/Subtitles/{index}/{routeFormat}/{routeFormat}",
            get(get_subtitle_with_ticks),
        )
        .route("/FallbackFont/Fonts", get(get_fallback_font_list))
        .route("/FallbackFont/Fonts/{name}", get(get_fallback_font))
        .route(
            "/Jellyfin.Plugin.OpenSubtitles/ValidateLoginInfo",
            post(validate_open_subtitles_login),
        )
}

/// `POST /Jellyfin.Plugin.OpenSubtitles/ValidateLoginInfo` — validate the
/// OpenSubtitles credentials the dashboard is about to save.
///
/// Ports the OpenSubtitles plugin's login-check action: the posted
/// `{ApiKey,Username,Password}` body is validated by attempting a real login via
/// the provider. `200` means the credentials work; a rejected login is `401`, a
/// missing key `400`.
async fn validate_open_subtitles_login(
    RequireAuth(_auth): RequireAuth,
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<StatusCode, ApiError> {
    state
        .subtitles
        .validate_provider_login("opensubtitles", &body)
        .await?;
    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use crate::handlers::image_upload::decode_base64;

    #[test]
    fn base64_round_trips_known_values() {
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("").unwrap(), b"");
        // Whitespace is ignored.
        assert_eq!(decode_base64("aGVs\nbG8=").unwrap(), b"hello");
    }

    #[test]
    fn base64_rejects_invalid_chars() {
        assert!(decode_base64("!!!!").is_none());
    }
}
