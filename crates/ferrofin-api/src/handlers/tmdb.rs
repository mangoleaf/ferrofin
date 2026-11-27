//! `TmdbController` — the TMDb plugin's client-configuration probe.
//!
//! Ports `GET /Tmdb/ClientConfiguration`, which surfaces the image portion of
//! TMDb's `/configuration` response: the TMDb settings page lists its image
//! sizes from it. As upstream's (`TmdbClientManager.GetClientConfiguration`
//! → `EnsureClientConfigAsync` → `TMDbClient.GetConfigAsync`), the first call
//! asks TMDb and the answer is kept for the process; a call TMDb does not
//! answer fails, and the next one asks again.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::dto::ConfigImageTypes;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// `GET /Tmdb/ClientConfiguration` — the TMDb image configuration options.
///
/// Port of `TmdbController.TmdbClientConfiguration`: the `Images` of TMDb's
/// client configuration, through the provider manager's TMDb client. A
/// configuration without images answers `204`, as ASP.NET does for a null
/// result; TMDb answering no configuration is upstream's thrown exception
/// (`GetConfigAsync`), answered as `ExceptionMiddleware` answers it.
#[utoipa::path(
    get,
    path = "/Tmdb/ClientConfiguration",
    responses(
        (status = 200, description = "TMDb image configuration returned", body = ConfigImageTypes),
        (status = 204, description = "TMDb's configuration has no images"),
        (status = 500, description = "TMDb answered no configuration"),
    ),
    tag = "ferrofin"
)]
async fn tmdb_client_configuration(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Response {
    match state.providers.tmdb_image_configuration().await {
        Ok(Some(images)) => Json(images).into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => ApiError::from(err).into_exception_response(),
    }
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/Tmdb/ClientConfiguration", get(tmdb_client_configuration))
}
