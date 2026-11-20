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
/// While the Single Sign-On plugin has sign-in buttons to show, they follow
/// the stored disclaimer ([`with_login_buttons`]); otherwise the answer is
/// the stored configuration, as upstream's.
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
    let login_disclaimer = match login_buttons(&state).await {
        Some(buttons) => Some(with_login_buttons(
            branding.login_disclaimer.as_deref(),
            &buttons,
        )),
        None => branding.login_disclaimer,
    };
    Ok(Json(BrandingOptionsDto {
        login_disclaimer,
        custom_css: branding.custom_css,
        splashscreen_enabled: branding.splashscreen_enabled,
    }))
}

/// Opens the Single Sign-On plugin's sign-in buttons in a served login
/// disclaimer. DOMPurify drops both markers from the login page.
const LOGIN_BUTTONS_BEGIN: &str = "<!-- ferrofin:sso-login-buttons -->";
/// Closes what [`LOGIN_BUTTONS_BEGIN`] opens.
const LOGIN_BUTTONS_END: &str = "<!-- /ferrofin:sso-login-buttons -->";

/// The Single Sign-On plugin's sign-in buttons
/// ([`SsoManager::login_buttons`](ferrofin_traits::sso::SsoManager::login_buttons)),
/// when it has any. A plugin configuration that does not read leaves them
/// out, so the login page still gets its branding. That is logged at `debug`
/// only: this anonymous route is asked on every login page, and the plugin's
/// settings page shows the admin the same failure.
async fn login_buttons(state: &AppState) -> Option<String> {
    match state.sso.as_ref()?.login_buttons().await {
        Ok(buttons) => buttons,
        Err(error) => {
            tracing::debug!(
                %error,
                "the login page's Single Sign-On buttons are left out: its configuration could \
                 not be read"
            );
            None
        }
    }
}

/// The served login disclaimer: the stored one, a blank line, so jellyfin-web's
/// markdown renderer reads what follows as an HTML block, then `buttons`
/// between the marker comments, which [`strip_login_buttons`] finds again
/// when the dashboard saves this text back (D7: the stored branding is never
/// changed).
fn with_login_buttons(stored: Option<&str>, buttons: &str) -> String {
    format!(
        "{}\n\n{LOGIN_BUTTONS_BEGIN}{buttons}{LOGIN_BUTTONS_END}",
        stored.unwrap_or_default()
    )
}

/// Removes every block of sign-in buttons [`with_login_buttons`] served from
/// a disclaimer about to be saved, with the blank line before it, in `\n`
/// line breaks (as jellyfin-web 12.4 sends them) or `\r\n` (as a form
/// submitted natively sends a textarea's). jellyfin-web's branding page
/// loads `GET /Branding/Configuration` into its editor and saves it back, and
/// the buttons must be neither stored nor served twice. A block runs from a
/// closing marker back to the nearest opening one; a marker without its pair
/// is left as it is.
pub(crate) fn strip_login_buttons(disclaimer: &mut Option<String>) {
    let Some(text) = disclaimer else {
        return;
    };
    let mut from = 0;
    while let Some(found) = text[from..].find(LOGIN_BUTTONS_END) {
        let close = from + found;
        let Some(open) = text[from..close].rfind(LOGIN_BUTTONS_BEGIN) else {
            from = close + LOGIN_BUTTONS_END.len();
            continue;
        };
        let open = from + open;
        let before = &text[..open];
        let start = ["\r\n\r\n", "\n\n"]
            .into_iter()
            .find_map(|blank| before.strip_suffix(blank))
            .map_or(open, str::len);
        text.replace_range(start..close + LOGIN_BUTTONS_END.len(), "");
        from = start;
    }
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

/// `File.Exists`: whether `path` names an existing file (a directory is not one).
///
/// The stat goes through `tokio::fs` (the blocking pool), never `std::fs` inline:
/// the login page fetches the splashscreen anonymously, and a cold stat on a
/// network-mounted data directory can park the worker thread.
async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
}

/// Resolves the splashscreen file path from the branding options.
///
/// Port of `GetSplashscreen`'s path resolution: the configured
/// `SplashscreenLocation` when it names an existing file, else
/// `<data>/splashscreen.png`. Returns `None` when neither exists.
async fn resolve_splashscreen_path(location: Option<&str>, data_path: &str) -> Option<String> {
    if let Some(loc) = location
        && !loc.trim().is_empty()
        && is_file(Path::new(loc)).await
    {
        return Some(loc.to_owned());
    }
    let default = Path::new(data_path).join("splashscreen.png");
    is_file(&default)
        .await
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
        .await
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
        &ImageQuery::splashscreen(query.tag, query.format),
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

    // With the whole body in, a client that hangs up stops neither the write
    // nor the save, as upstream's copy (`CancellationToken.None`) and its
    // synchronous save run on.
    crate::handlers::run_detached(async move {
        let data_path = state.config.application_paths().data_path();
        // Jellyfin's data path exists from startup; a fresh Ferrofin data
        // directory has no such folder until something writes to it.
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
        crate::handlers::config::run_committed_checks(&state, "branding")
    })
    .await?;
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
    // A synchronous action upstream: a client that hangs up cannot leave the
    // file deleted and its location still recorded.
    crate::handlers::run_detached(async move {
        let mut branding = state.config.get_branding().await?;
        if let Some(loc) = branding.splashscreen_location.clone()
            && !loc.trim().is_empty()
            && is_file(Path::new(&loc)).await
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
        Ok::<_, ApiError>(())
    })
    .await?;
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
    SplashscreenQuery {} => [("get", "/Branding/Splashscreen")];
}
