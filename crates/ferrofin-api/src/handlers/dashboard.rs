//! `DashboardController` — plugin configuration-page discovery.
//!
//! Ports the two dashboard reads:
//! - `GET /web/ConfigurationPages` — the plugin configuration pages, projected
//!   from each registered plugin's `config_pages` (the C# projects these from
//!   `IHasWebPages`). jellyfin-web matches a page to its plugin strictly by
//!   `PluginId` and labels it with `DisplayName`, so both are always set.
//! - `GET /web/ConfigurationPage` — a single page's HTML/JS resource by
//!   (case-insensitive) name, MIME-typed from the name's extension; unknown
//!   names return `404` like the C#. Also registered under the lowercase
//!   `configurationpage` spelling jellyfin-web actually requests.

use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::plugins::ConfigurationPageInfo;

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::state::AppState;

/// Query parameters for `GET /web/ConfigurationPages`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigurationPagesQuery {
    /// Optional filter on whether a page is enabled in the main menu.
    #[serde(default)]
    enable_in_main_menu: Option<bool>,
}

/// `GET /web/ConfigurationPages` — the registered plugins' configuration pages.
///
/// Port of `DashboardController.GetConfigurationPages`.
#[utoipa::path(
    get,
    path = "/web/ConfigurationPages",
    params(("enableInMainMenu" = Option<bool>, Query, description = "Whether to enable in the main menu.")),
    responses((status = 200, description = "ConfigurationPages returned", body = [ConfigurationPageInfo])),
    tag = "ferrofin"
)]
async fn get_configuration_pages(
    State(state): State<AppState>,
    _auth: RequireAdmin,
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
/// Port of `DashboardController.GetDashboardConfigurationPage`: the page whose
/// name matches `name` case-insensitively, served with the MIME type its
/// resource name implies. Anonymous, as upstream — the singular action carries
/// no `[Authorize]` and Jellyfin registers no fallback policy.
#[utoipa::path(
    get,
    path = "/web/ConfigurationPage",
    params(("name" = Option<String>, Query, description = "The name of the page.")),
    responses(
        (status = 200, description = "ConfigurationPage returned"),
        (status = 404, description = "Plugin configuration page not found")
    ),
    tag = "ferrofin"
)]
async fn get_dashboard_configuration_page(
    State(state): State<AppState>,
    Query(query): Query<ConfigurationPageQuery>,
) -> Result<Response, ApiError> {
    let name = query.name.unwrap_or_default();
    match state.plugins.get_configuration_page(&name).await? {
        Some(bytes) => Ok(([(header::CONTENT_TYPE, page_mime(&name))], bytes).into_response()),
        None => Err(ApiError::NotFound(format!(
            "configuration page {name:?} not found"
        ))),
    }
}

/// The MIME type for a configuration page resource, from its name's extension.
///
/// A plugin page's shell HTML loads sibling resources by name (e.g.
/// `configurationpage?name=introskipper.js` as a `<script type="module">`) —
/// browsers refuse module scripts served as `text/html`, so the extension must
/// drive the content type. Nameless / extensionless pages are the HTML page.
fn page_mime(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or_default() {
        "js" | "mjs" => "application/javascript",
        "css" => "text/css",
        "json" => "application/json",
        _ => "text/html; charset=utf-8",
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
        // ASP.NET routing is case-insensitive; axum's is not. jellyfin-web's
        // plugin shell pages reference sibling resources with the lowercase
        // `configurationpage?name=…`, so serve that spelling too.
        .route(
            "/web/configurationpage",
            get(get_dashboard_configuration_page),
        )
}
