//! Audio streaming routes, direct play and progressive transcoding.
//! Ordinary stream requests honour Static (default false), requested codecs,
//! output container and HEAD. Universal-audio negotiation remains separate.

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::response::Response;
use axum::routing::get;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::Query;
use crate::handlers::items::{require_visible_item, resolve_user_opt};
use crate::handlers::streaming::{serve_static_file, stream_path};
use crate::state::AppState;

/// `GET`/`HEAD /Audio/{itemId}/stream` — serve the item's audio file.
///
/// Port of `AudioController.GetAudioStream` (direct-stream path only). Range
/// requests yield `206 Partial Content`; `HEAD` returns headers only.
async fn get_audio_stream(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<ferrofin_traits::options::AuthorizationInfo>,
    Path(item_id): Path<Uuid>,
    Query(query): Query<crate::handlers::hls::HlsQueryPub>,
    request: Request,
) -> Result<Response, ApiError> {
    let raw = request.uri().query().map(ToOwned::to_owned);
    let req = crate::handlers::hls::request_from_query(item_id, query, raw, &auth);
    crate::handlers::hls::validate_stream_request(&req)?;
    if req.is_static {
        let path = stream_path(&state, item_id).await?;
        return serve_static_file(&path, request).await;
    }
    crate::handlers::hls::transcode_stream_fallback(&state, item_id, true, req, request).await
}

/// The extension route has the same Static=false default as the bare route.
async fn get_audio_stream_by_container(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<ferrofin_traits::options::AuthorizationInfo>,
    Path((item_id, container)): Path<(Uuid, String)>,
    Query(query): Query<crate::handlers::hls::HlsQueryPub>,
    request: Request,
) -> Result<Response, ApiError> {
    let raw = request.uri().query().map(ToOwned::to_owned);
    let mut req = crate::handlers::hls::request_from_query(item_id, query, raw, &auth);
    req.output_container = Some(
        container
            .strip_prefix("stream.")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::NotFound("stream route".to_owned()))?
            .to_owned(),
    );
    crate::handlers::hls::validate_stream_request(&req)?;
    if req.is_static {
        let path = stream_path(&state, item_id).await?;
        return serve_static_file(&path, request).await;
    }
    crate::handlers::hls::transcode_stream_fallback(&state, item_id, true, req, request).await
}

/// The `userId` query parameter `GET /Audio/{itemId}/universal` accepts.
///
/// Upstream resolves it through `RequestHelpers.GetUserId`
/// (v10.11.8 `UniversalAudioController.cs:120`) before it builds the playback
/// info, so naming another user as a non-administrator is a `403` there. The
/// stream itself is item-scoped here, but dropping the parameter turned that
/// refusal into a served stream.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UniversalAudioUserQuery {
    /// Optional target user; defaults to the authenticated caller.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
}

/// `GET`/`HEAD /Audio/{itemId}/universal` — serve the item's audio file.
///
/// Port of `UniversalAudioController.GetUniversalAudioStream`, direct-play branch:
/// the resolved source supports direct stream, so the original file is served
/// progressively (Range/`HEAD`). Requires authentication (`[Authorize]`).
async fn get_universal_audio_stream(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(hls_query): Query<crate::handlers::hls::UniversalHlsQueryPub>,
    Query(user_query): Query<UniversalAudioUserQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    let user = resolve_user_opt(&state, &auth, user_query.user_id).await?;
    require_visible_item(&state, item_id, user.as_ref()).await?;
    // Direct-play when a static source exists; otherwise transcode (the
    // UniversalAudioController fallback), now wired to the real runtime.
    match stream_path(&state, item_id).await {
        Ok(path) => serve_static_file(&path, request).await,
        Err(ApiError::NotFound(_)) => {
            let raw = request.uri().query().map(ToOwned::to_owned);
            let req =
                crate::handlers::hls::request_from_universal_query(item_id, hls_query, raw, &auth);
            crate::handlers::hls::transcode_stream_fallback(&state, item_id, true, req, request)
                .await
        }
        Err(other) => Err(other),
    }
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Audio/{itemId}/stream",
            get(get_audio_stream).head(get_audio_stream),
        )
        .route(
            "/Audio/{itemId}/{container}",
            get(get_audio_stream_by_container).head(get_audio_stream_by_container),
        )
        .route(
            "/Audio/{itemId}/universal",
            get(get_universal_audio_stream).head(get_universal_audio_stream),
        )
}
crate::query::query_parameters! {
    UniversalAudioUserQuery {} => [("get", "/Audio/{itemId}/universal")];
}
