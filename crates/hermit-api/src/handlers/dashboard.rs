//! `DashboardController` — plugin configuration-page discovery.
//!
//! Ports the two dashboard reads:
//! - `GET /web/ConfigurationPages` — the plugin configuration pages. Hermit ships
//!   no dynamic plugin host, so the page list is always empty (the C# projects
//!   these from installed plugins' `IHasWebPages`).
//! - `GET /web/ConfigurationPage` — a single page's HTML/JS resource. With no
//!   plugins there is never a matching page, so this returns `404` (the same
//!   `NotFound` the C# returns for an unknown page name).
//!
//! `GET /web/ConfigurationPages` is elevation-gated; both collapse to the
//! contract's routing here.

use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router};
use hermit_model::plugins::ConfigurationPageInfo;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// Query parameters for `GET /web/ConfigurationPages`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigurationPagesQuery {
    /// Optional filter on whether a page is enabled in the main menu. Unused
    /// while the page list is empty, but accepted per the contract.
    #[serde(default)]
    enable_in_main_menu: Option<bool>,
}

/// `GET /web/ConfigurationPages` — the plugin configuration pages (always empty).
///
/// Port of `DashboardController.GetConfigurationPages`.
#[utoipa::path(
    get,
    path = "/web/ConfigurationPages",
    params(("enableInMainMenu" = Option<bool>, Query, description = "Whether to enable in the main menu.")),
    responses((status = 200, description = "ConfigurationPages returned", body = [ConfigurationPageInfo])),
    tag = "hermit"
)]
async fn get_configuration_pages(
    State(state): State<AppState>,
    _auth: RequireAuth,
    Query(query): Query<ConfigurationPagesQuery>,
) -> Result<Json<Vec<ConfigurationPageInfo>>, ApiError> {
    let mut pages = state.plugins.get_configuration_pages().await?;
    // When the caller filters on main-menu placement, keep only matching pages.
    if let Some(want) = query.enable_in_main_menu {
        pages.retain(|p| p.enable_in_main_menu == want);
    }
    Ok(Json(pages))
}

/// Query parameters for `GET /web/ConfigurationPage`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigurationPageQuery {
    /// The name of the page to fetch.
    #[serde(default)]
    name: Option<String>,
}

/// `GET /web/ConfigurationPage` — a single dashboard configuration page.
///
/// Port of `DashboardController.GetDashboardConfigurationPage`: with no plugin
/// pages installed, no page ever matches, so this always returns `404`.
#[utoipa::path(
    get,
    path = "/web/ConfigurationPage",
    params(("name" = Option<String>, Query, description = "The name of the page.")),
    responses(
        (status = 200, description = "ConfigurationPage returned"),
        (status = 404, description = "Plugin configuration page not found")
    ),
    tag = "hermit"
)]
async fn get_dashboard_configuration_page(
    State(state): State<AppState>,
    Query(query): Query<ConfigurationPageQuery>,
) -> Result<Html<Vec<u8>>, ApiError> {
    let name = query.name.unwrap_or_default();
    match state.plugins.get_configuration_page(&name).await? {
        Some(html) => Ok(Html(html)),
        None => Err(ApiError::NotFound(format!(
            "configuration page {name:?} not found"
        ))),
    }
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/web/ConfigurationPages", get(get_configuration_pages))
        .route(
            "/web/ConfigurationPage",
            get(get_dashboard_configuration_page),
        )
}
