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
//!   when splashscreen is disabled (an administrator may still preview it) or no
//!   file exists. `format` converts the image and `tag` earns the immutable
//!   caching contract, through the same tail as [`super::images`].
//! - `POST /Branding/Splashscreen` — upload a custom splashscreen: base64 body +
//!   image `Content-Type`, written to `<data>/splashscreen-upload{ext}`, path
//!   recorded on the branding options.
//! - `DELETE /Branding/Splashscreen` — remove the custom splashscreen file and
//!   clear its recorded location.

use std::path::Path;

use axum::extract::{Request, State};
use axum::handler::Handler;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::branding::BrandingOptionsDto;
use ferrofin_traits::options::ItemImageInfo;

use crate::auth::{AuthenticatedIdentity, RequireAdmin, is_administrator_identity};
use crate::error::ApiError;
use crate::extract::{Query, upload_body_limit};
use crate::handlers::image_upload::{decode_base64, try_get_image_extension_from_content_type};
use crate::handlers::images::{ImageQuery, serve_image_file};
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
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SplashscreenQuery {
    /// Optional cache tag: its presence makes the response immutable-cacheable.
    #[serde(default)]
    tag: Option<String>,
    /// Optional output format the image is converted to.
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
/// Port of `ImageController.GetSplashscreen`: `404` when splashscreen is disabled
/// (unless the caller is an administrator) or when no splashscreen file exists;
/// otherwise the stored file is served through the image processor.
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
    auth: Result<AuthenticatedIdentity, ApiError>,
    Query(query): Query<SplashscreenQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    let branding = state.config.get_branding().await?;
    // `!SplashscreenEnabled && !User.IsInRole(Administrator)`: an administrator
    // may preview a disabled splashscreen (the dashboard's upload page does).
    if !branding.splashscreen_enabled {
        let is_admin = match &auth {
            Ok(identity) => is_administrator_identity(&state, &identity.0).await?,
            Err(_) => false,
        };
        if !is_admin {
            return Err(ApiError::NotFound("splashscreen is disabled".to_owned()));
        }
    }
    let data_path = state.config.application_paths().data_path();
    let path = resolve_splashscreen_path(branding.splashscreen_location.as_deref(), &data_path)
        .ok_or_else(|| ApiError::NotFound("no splashscreen file".to_owned()))?;
    // The same tail as every image route: the processor converts to the
    // requested `format`, and a `tag` earns the year-long immutable caching
    // (`GetImageResult` with a 365-day cache duration).
    let modified = tokio::fs::metadata(&path)
        .await
        .and_then(|meta| meta.modified())
        .map_or_else(
            |_| chrono::Utc::now(),
            chrono::DateTime::<chrono::Utc>::from,
        );
    let image = ItemImageInfo {
        path,
        image_type: ferrofin_model::entities::ImageType::Primary,
        date_modified: modified,
        width: 0,
        height: 0,
        blur_hash: None,
    };
    serve_image_file(
        &state,
        uuid::Uuid::nil(),
        &image,
        0,
        false,
        &ImageQuery::tagged(query.tag, query.format),
        request,
    )
    .await
}

/// `POST /Branding/Splashscreen` — upload a custom splashscreen.
///
/// Port of `ImageController.UploadCustomSplashscreen`: validates the
/// `Content-Type` with `TryGetImageExtensionFromContentType` (an image type
/// `ToExtension` knows, else `400 Incorrect ContentType.`), base64-decodes the body,
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
    _auth: RequireAdmin,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<StatusCode, ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let extension = try_get_image_extension_from_content_type(content_type)
        .ok_or_else(|| ApiError::BadRequest("Incorrect ContentType.".to_owned()))?
        .extension;
    let bytes = decode_base64(body.trim())
        .ok_or_else(|| ApiError::BadRequest("splashscreen data is not valid base64".to_owned()))?;

    let data_path = state.config.application_paths().data_path();
    // Jellyfin's data path exists from startup; a fresh Ferrofin data directory
    // has no such folder until something writes to it.
    tokio::fs::create_dir_all(&data_path).await.map_err(|e| {
        ApiError::Service(ferrofin_traits::error::ServiceError::backend(format!(
            "create data directory: {e}"
        )))
    })?;
    let file_path = Path::new(&data_path).join(format!("splashscreen-upload{extension}"));
    tokio::fs::write(&file_path, &bytes).await.map_err(|e| {
        ApiError::Service(ferrofin_traits::error::ServiceError::backend(format!(
            "write splashscreen: {e}"
        )))
    })?;

    let mut branding = state.config.get_branding().await?;
    branding.splashscreen_location = Some(file_path.to_string_lossy().into_owned());
    state.config.update_branding(&branding).await?;
    crate::handlers::config::run_committed_checks(&state, "branding")?;
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
async fn delete_splashscreen(
    State(state): State<AppState>,
    _auth: RequireAdmin,
) -> Result<StatusCode, ApiError> {
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
        crate::handlers::config::run_committed_checks(&state, "branding")?;
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
                .post(upload_splashscreen.layer(upload_body_limit()))
                .delete(delete_splashscreen),
        )
}
crate::query::query_parameters! {
    SplashscreenQuery {} => [("get", "/Branding/Css")];
}
