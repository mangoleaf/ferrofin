//! `BrandingController` + `ImageController` splashscreen — branding config, CSS,
//! and the splashscreen image asset.
//!
//! Ports the anonymous branding reads:
//! - `GET /Branding/Configuration` — the [`BrandingOptionsDto`] (the API view,
//!   excluding `SplashscreenLocation`).
//! - `GET /Branding/Css` and `GET /Branding/Css.css` — the configured custom CSS
//!   as `text/css` (empty string when unset).
//!
//! And the splashscreen asset routes (`ImageController` in Jellyfin):
//! - `GET /Branding/Splashscreen` — serves the splashscreen image file (the
//!   configured `SplashscreenLocation`, else `<data>/splashscreen.png`); `404`
//!   when splashscreen is disabled or no file exists. Under original-file serving
//!   the `tag`/`format` query parameters are accepted but do not transform the
//!   bytes (as in [`super::images`]).
//! - `POST /Branding/Splashscreen` — upload a custom splashscreen: base64 body +
//!   image `Content-Type`, written to `<data>/splashscreen-upload{ext}`, path
//!   recorded on the branding options.
//! - `DELETE /Branding/Splashscreen` — remove the custom splashscreen file and
//!   clear its recorded location.

use std::path::Path;

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::branding::BrandingOptionsDto;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::error::ApiError;
use crate::handlers::image_upload::{decode_base64, image_extension_from_content_type};
use crate::state::AppState;

/// `GET /Branding/Configuration` — the branding configuration DTO.
///
/// Port of `BrandingController.GetBrandingOptions`: projects the stored
/// `BrandingOptions` into the API DTO (dropping `SplashscreenLocation`).
#[utoipa::path(
    get,
    path = "/Branding/Configuration",
    responses((status = 200, description = "Branding configuration returned", body = BrandingOptionsDto)),
    tag = "ferrofin"
)]
async fn get_branding_options(
    State(state): State<AppState>,
) -> Result<Json<BrandingOptionsDto>, ApiError> {
    let branding = state.config.get_branding().await?;
    Ok(Json(BrandingOptionsDto {
        login_disclaimer: branding.login_disclaimer,
        custom_css: branding.custom_css,
        splashscreen_enabled: branding.splashscreen_enabled,
    }))
}

/// `GET /Branding/Css` (and `/Branding/Css.css`) — the configured custom CSS.
///
/// Port of `BrandingController.GetBrandingCss`: returns the custom CSS as
/// `text/css`, or the empty string when none is configured.
#[utoipa::path(
    get,
    path = "/Branding/Css",
    responses(
        (status = 200, description = "Branding css returned"),
        (status = 204, description = "No branding css configured")
    ),
    tag = "ferrofin"
)]
async fn get_branding_css(State(state): State<AppState>) -> Result<Response, ApiError> {
    let branding = state.config.get_branding().await?;
    let css = branding.custom_css.unwrap_or_default();
    let mut response = css.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/css"));
    Ok(response)
}

/// The query parameters accepted by `GET /Branding/Splashscreen`.
///
/// `tag`/`format` mirror the C# signature; under original-file serving they are
/// accepted (so a client sending them still gets a `200`) but do not transform
/// the bytes — hence unread, like the sibling image routes' `ImageQuery` fields.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct SplashscreenQuery {
    /// Optional cache tag (accepted, not used under original-file serving).
    #[serde(default)]
    tag: Option<String>,
    /// Optional output format (accepted, not used under original-file serving).
    #[serde(default)]
    format: Option<String>,
}

/// Resolves the splashscreen file path from the branding options.
///
/// Port of `GetSplashscreen`'s path resolution: the configured
/// `SplashscreenLocation` when it names an existing file, else
/// `<data>/splashscreen.png`. Returns `None` when neither exists.
fn resolve_splashscreen_path(location: Option<&str>, data_path: &str) -> Option<String> {
    if let Some(loc) = location
        && !loc.trim().is_empty()
        && Path::new(loc).is_file()
    {
        return Some(loc.to_owned());
    }
    let default = Path::new(data_path).join("splashscreen.png");
    default
        .is_file()
        .then(|| default.to_string_lossy().into_owned())
}

/// `GET /Branding/Splashscreen` — serve the splashscreen image.
///
/// Port of `ImageController.GetSplashscreen`: `404` when splashscreen is disabled,
/// or when no splashscreen file exists; otherwise the stored file is served (the
/// image processor is not ported, so the original bytes are returned).
#[utoipa::path(
    get,
    path = "/Branding/Splashscreen",
    params(
        ("tag" = Option<String>, Query, description = "Optional cache tag"),
        ("format" = Option<String>, Query, description = "Optional output format"),
    ),
    responses(
        (status = 200, description = "Splashscreen returned"),
        (status = 404, description = "Splashscreen disabled or not found"),
    ),
    tag = "ferrofin"
)]
async fn get_splashscreen(
    State(state): State<AppState>,
    Query(_query): Query<SplashscreenQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    let branding = state.config.get_branding().await?;
    // The C# admin bypass of the disabled flag needs the elevation policy (not
    // wired at this seam); the disabled case is the not-found outcome.
    if !branding.splashscreen_enabled {
        return Err(ApiError::NotFound("splashscreen is disabled".to_owned()));
    }
    let data_path = state.config.application_paths().data_path();
    let path = resolve_splashscreen_path(branding.splashscreen_location.as_deref(), &data_path)
        .ok_or_else(|| ApiError::NotFound("no splashscreen file".to_owned()))?;
    let response = ServeFile::new(&path)
        .oneshot(request)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;
    Ok(response.map(Body::new))
}

/// `POST /Branding/Splashscreen` — upload a custom splashscreen.
///
/// Port of `ImageController.UploadCustomSplashscreen`: validates the
/// `Content-Type` is an image type (`400` otherwise), base64-decodes the body,
/// writes it to `<data>/splashscreen-upload{ext}`, and records that path on the
/// branding options. Returns `204`.
#[utoipa::path(
    post,
    path = "/Branding/Splashscreen",
    responses(
        (status = 204, description = "Splashscreen uploaded"),
        (status = 400, description = "Incorrect content type"),
    ),
    tag = "ferrofin"
)]
async fn upload_splashscreen(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<StatusCode, ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let extension = image_extension_from_content_type(content_type)
        .ok_or_else(|| ApiError::BadRequest("Incorrect ContentType.".to_owned()))?;
    let bytes = decode_base64(body.trim())
        .ok_or_else(|| ApiError::BadRequest("splashscreen data is not valid base64".to_owned()))?;

    let data_path = state.config.application_paths().data_path();
    let file_path = Path::new(&data_path).join(format!("splashscreen-upload{extension}"));
    tokio::fs::write(&file_path, &bytes).await.map_err(|e| {
        ApiError::Service(ferrofin_traits::error::ServiceError::backend(format!(
            "write splashscreen: {e}"
        )))
    })?;

    let mut branding = state.config.get_branding().await?;
    branding.splashscreen_location = Some(file_path.to_string_lossy().into_owned());
    state.config.update_branding(&branding).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /Branding/Splashscreen` — remove the custom splashscreen.
///
/// Port of `ImageController.DeleteCustomSplashscreen`: when a
/// `SplashscreenLocation` file exists, delete it and clear the recorded location;
/// idempotent otherwise. Returns `204`.
#[utoipa::path(
    delete,
    path = "/Branding/Splashscreen",
    responses((status = 204, description = "Splashscreen deleted")),
    tag = "ferrofin"
)]
async fn delete_splashscreen(State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    let mut branding = state.config.get_branding().await?;
    if let Some(loc) = branding.splashscreen_location.clone()
        && !loc.trim().is_empty()
        && Path::new(&loc).is_file()
    {
        tokio::fs::remove_file(&loc).await.map_err(|e| {
            ApiError::Service(ferrofin_traits::error::ServiceError::backend(format!(
                "delete splashscreen: {e}"
            )))
        })?;
        branding.splashscreen_location = None;
        state.config.update_branding(&branding).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Branding/Configuration", get(get_branding_options))
        .route("/Branding/Css", get(get_branding_css))
        .route("/Branding/Css.css", get(get_branding_css))
        .route(
            "/Branding/Splashscreen",
            get(get_splashscreen)
                .post(upload_splashscreen)
                .delete(delete_splashscreen),
        )
}
