//! `BrandingController` — branding configuration + custom CSS.
//!
//! Ports the anonymous branding reads:
//! - `GET /Branding/Configuration` — the [`BrandingOptionsDto`] (the API view,
//!   excluding `SplashscreenLocation`).
//! - `GET /Branding/Css` and `GET /Branding/Css.css` — the configured custom CSS
//!   as `text/css` (empty string when unset).
//!
//! The `/Branding/Splashscreen` GET/POST/DELETE routes drive the splashscreen
//! image asset and stay on the `501` stub (image-generation subsystem, deferred).

use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use hermit_model::branding::BrandingOptionsDto;

use crate::error::ApiError;
use crate::state::AppState;

/// `GET /Branding/Configuration` — the branding configuration DTO.
///
/// Port of `BrandingController.GetBrandingOptions`: projects the stored
/// `BrandingOptions` into the API DTO (dropping `SplashscreenLocation`).
#[utoipa::path(
    get,
    path = "/Branding/Configuration",
    responses((status = 200, description = "Branding configuration returned", body = BrandingOptionsDto)),
    tag = "hermit"
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
    tag = "hermit"
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

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Branding/Configuration", get(get_branding_options))
        .route("/Branding/Css", get(get_branding_css))
        .route("/Branding/Css.css", get(get_branding_css))
}
