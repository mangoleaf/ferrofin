//! The Single Sign-On plugin's provider settings and sign-in routes.
//!
//! The HTTP seam of the **Single Sign-On extension**
//! (`ferrofin-extensions::sso`), Ferrofin's own OIDC sign-in plugin. The
//! configuration, the secret store, the sign-in flow and its checks live with
//! the extension; this handler is the thin seam over the [`SsoManager`] trait:
//! - `GET /sso/admin/oidc/providers` — the providers, each with whether a
//!   client secret is stored and its callback URL, never a secret;
//! - `PUT /sso/admin/oidc/providers/{provider}` — create or update one, then
//!   test its discovery; a body without `ClientSecret` keeps the stored
//!   secret;
//! - `DELETE /sso/admin/oidc/providers/{provider}` — remove one and its secret;
//! - `GET /sso/oidc/{provider}/start` and `GET /sso/oidc/{provider}/callback`
//!   — a sign-in at the provider, for anyone (D2).
//!
//! Not Jellyfin contract operations, so they are listed in
//! [`PLUGIN_ROUTES`](super::PLUGIN_ROUTES) rather than in `REAL_ROUTES` or
//! `EXTENSION_ROUTES` (both hold contract operations only), and carry no
//! `VERIFIED` row: there is no upstream to compare them with (D2, D15).
//!
//! The settings routes are for administrators only. While the plugin is
//! disabled, or the extension is not wired, every route answers `404` to
//! every caller, before it looks at the credentials, the body or the query: a
//! server without the plugin has no such route (B1).

use std::sync::Arc;

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{OidcProvider, OidcProviderInfo, OidcProviderList};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::sso::{OIDC_FLOW_COOKIE, OidcCallback, SsoManager, SsoReply};

use crate::auth::RequireAdmin;
use crate::error::{ApiError, FailureLogged};
use crate::extract::{JsonBody, ProblemDetails};
use crate::state::AppState;

/// The wired Single Sign-On service while its plugin is enabled, or the `404`
/// a server without the plugin answers.
async fn enabled_service(state: &AppState) -> Result<Arc<dyn SsoManager>, ApiError> {
    let sso = state.sso.clone().ok_or_else(|| {
        ApiError::from(ServiceError::not_found(
            "the Single Sign-On plugin is not available",
        ))
    })?;
    sso.ensure_enabled().await?;
    Ok(sso)
}

/// `GET /sso/admin/oidc/providers` — the plugin's public URL setting and its
/// OIDC providers, each with whether a client secret is stored, its callback
/// URL when a public URL is known, and why it is refused when it is. No answer
/// holds a client secret.
#[utoipa::path(
    get,
    path = "/sso/admin/oidc/providers",
    responses(
        (status = 200, description = "The OIDC providers", body = OidcProviderList),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "The Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn list_oidc_providers(
    State(state): State<AppState>,
    admin: Result<RequireAdmin, ApiError>,
) -> Result<Json<OidcProviderList>, ApiError> {
    let sso = enabled_service(&state).await?;
    admin?;
    Ok(Json(sso.oidc_providers().await?))
}

/// `PUT /sso/admin/oidc/providers/{provider}` — creates the provider
/// `{provider}`, or replaces its settings, then tests its discovery, and
/// answers it as the list shows it with the test's result as `Discovery`: the
/// endpoints found, or why the test failed. A failed test still saves, and
/// still answers `200`.
///
/// A body without `ClientSecret`, or with it empty or blank, keeps the
/// stored secret; a provider with none stored needs one. A refused setting is
/// a `400` naming it, and is not tested; a body that does not bind is a `400`
/// validation problem.
#[utoipa::path(
    put,
    path = "/sso/admin/oidc/providers/{provider}",
    params(("provider" = String, Path, description = "The provider's key: 1 to 32 lowercase letters, digits or hyphens")),
    request_body = OidcProvider,
    responses(
        (status = 200, description = "The provider was saved, and its discovery tested", body = OidcProviderInfo),
        (status = 400, description = "A setting is refused, or the body does not bind"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "The Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn save_oidc_provider(
    State(state): State<AppState>,
    admin: Result<RequireAdmin, ApiError>,
    provider: Result<Path<String>, PathRejection>,
    body: Result<JsonBody<OidcProvider>, ProblemDetails>,
) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    admin?;
    let provider = match provider {
        Ok(Path(provider)) => provider,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    let settings = match body {
        Ok(JsonBody(settings)) => settings,
        Err(problem) => return Ok(problem.into_response()),
    };
    let mut saved = sso.save_oidc_provider(&provider, settings).await?;
    // Saved, then tested, as LDAP's connection test saves before it binds: a
    // failed test is in the answer, never its status.
    saved.discovery = Some(sso.test_oidc_provider(&saved.provider).await?);
    Ok(Json(saved).into_response())
}

/// `DELETE /sso/admin/oidc/providers/{provider}` — removes the provider and
/// its client secret.
#[utoipa::path(
    delete,
    path = "/sso/admin/oidc/providers/{provider}",
    params(("provider" = String, Path, description = "The provider's key")),
    responses(
        (status = 204, description = "The provider was removed"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "No such provider, or the Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn delete_oidc_provider(
    State(state): State<AppState>,
    admin: Result<RequireAdmin, ApiError>,
    provider: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    admin?;
    let Path(provider) = match provider {
        Ok(path) => path,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    sso.delete_oidc_provider(&provider).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The provider key of a sign-in route: one that does not decode names no
/// provider, so it is the `404` of an unknown one.
fn sign_in_provider(provider: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    provider
        .map(|Path(provider)| provider)
        .map_err(|_| ServiceError::not_found("no such OIDC provider").into())
}

/// `reply` as the browser gets it. Neither answer may be cached: each holds a
/// one-time flow, and the callback's URL holds the code, which no `Referer`
/// carries on.
fn browser_reply(reply: SsoReply) -> Result<Response, ApiError> {
    let status = StatusCode::from_u16(reply.status)
        .map_err(|_| ServiceError::backend("the Single Sign-On reply has no valid status"))?;
    let mut response = match reply.location {
        Some(location) => (status, [(header::LOCATION, location)]).into_response(),
        None => (status, Html(reply.html)).into_response(),
    };
    if status.is_server_error() {
        // The plugin logged why: the request span must not log it again
        // (LOGGING.md, "Log errors once").
        response.extensions_mut().insert(FailureLogged);
    }
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    for cookie in reply.set_cookies {
        let cookie = HeaderValue::try_from(cookie).map_err(|_| {
            ServiceError::backend("the Single Sign-On cookie is not a header value")
        })?;
        headers.append(header::SET_COOKIE, cookie);
    }
    Ok(response)
}

/// `GET /sso/oidc/{provider}/start` — sends the browser to the provider's
/// sign-in page, binding the attempt to it with the [`OIDC_FLOW_COOKIE`].
/// Anonymous: this is how a user signs in.
#[utoipa::path(
    get,
    path = "/sso/oidc/{provider}/start",
    params(("provider" = String, Path, description = "The provider's key")),
    responses(
        (status = 302, description = "To the identity provider, with the flow cookie set"),
        (status = 502, description = "The identity provider could not be reached: a page saying so"),
        (status = 503, description = "No public URL is set to build the callback URL on: a page naming the setting"),
        (status = 404, description = "No such enabled provider, or the Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn start_oidc_sign_in(
    State(state): State<AppState>,
    provider: Result<Path<String>, PathRejection>,
    parts: Parts,
) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    let provider = sign_in_provider(provider)?;
    let client = state.client_address(&parts);
    browser_reply(sso.start_oidc_sign_in(&provider, client).await?)
}

/// `GET /sso/oidc/{provider}/callback` — where the identity provider sends
/// the browser back. A response that passes every check ends on a page naming
/// the verified identity; any other on the refusal page. Either way the flow
/// cookie is cleared.
#[utoipa::path(
    get,
    path = "/sso/oidc/{provider}/callback",
    params(
        ("provider" = String, Path, description = "The provider's key"),
        ("state" = Option<String>, Query, description = "Names the pending sign-in"),
        ("code" = Option<String>, Query, description = "The authorization code"),
        ("iss" = Option<String>, Query, description = "The issuer that answered (RFC 9207)"),
        ("error" = Option<String>, Query, description = "Why the identity provider refused")
    ),
    responses(
        (status = 200, description = "The verified identity"),
        (status = 400, description = "Refused: the sign-in failed a check"),
        (status = 502, description = "Refused: the identity provider could not be reached"),
        (status = 404, description = "No such enabled provider, or the Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn finish_oidc_sign_in(
    State(state): State<AppState>,
    provider: Result<Path<String>, PathRejection>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    let provider = sign_in_provider(provider)?;
    let pairs: Vec<(String, String)> =
        form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
            .into_owned()
            .collect();
    // A callback that repeats one of its parameters is no answer to trust:
    // it reads as having none, which the plugin refuses.
    let names = ["state", "code", "iss", "error"];
    let repeated = names
        .iter()
        .any(|name| pairs.iter().filter(|(n, _)| n == name).count() > 1);
    let once = |name: &str| {
        pairs
            .iter()
            .find(|(n, _)| n == name)
            .filter(|_| !repeated)
            .map(|(_, value)| value.clone())
    };
    let flow_cookie = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == OIDC_FLOW_COOKIE).then(|| Secret::new(value))
        });
    let callback = OidcCallback {
        state: once("state"),
        code: once("code").map(Secret::new),
        iss: once("iss"),
        error: once("error"),
        flow_cookie,
    };
    browser_reply(sso.finish_oidc_sign_in(&provider, callback).await?)
}

/// Registers this controller's routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/sso/admin/oidc/providers", get(list_oidc_providers))
        .route(
            "/sso/admin/oidc/providers/{provider}",
            put(save_oidc_provider).delete(delete_oidc_provider),
        )
        .route("/sso/oidc/{provider}/start", get(start_oidc_sign_in))
        .route("/sso/oidc/{provider}/callback", get(finish_oidc_sign_in))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `5xx` page the plugin logged is not logged again by the request
    /// span; any other answer is left to it.
    #[test]
    fn a_server_error_page_is_marked_as_logged() {
        let page = |status| SsoReply {
            status,
            location: None,
            html: "<p>Sign-in failed. Try again.</p>".to_owned(),
            set_cookies: Vec::new(),
        };
        for (status, marked) in [(502, true), (503, true), (400, false), (200, false)] {
            let response = browser_reply(page(status)).expect("a response");
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(
                response.extensions().get::<FailureLogged>().is_some(),
                marked,
                "{status}"
            );
        }
    }
}
