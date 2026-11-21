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
//!   — a sign-in at the provider, for anyone (D2), which ends on the
//!   completion page;
//! - `POST /sso/oidc/exchange` — the completion page's script swaps its
//!   one-time code for the session there, for anyone;
//! - `GET /sso/quickconnect` and `POST /sso/quickconnect` — a native app's
//!   Quick Connect code approved through a sign-in started with an empty
//!   `?quickConnect`, then typed on the confirmation page, for anyone;
//! - the account-link routes, in [`links`].
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
use axum::routing::{get, post, put};
use axum::{Json, Router};
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{OidcExchangeRequest, OidcProvider, OidcProviderInfo, OidcProviderList};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::session::AuthenticationRequest;
use ferrofin_traits::sso::{OidcCallback, SsoManager, SsoReply};

use crate::auth::RequireAdmin;
use crate::error::{ApiError, FailureLogged};
use crate::extract::{JsonBody, ProblemDetails};
use crate::state::AppState;

pub(crate) mod links;

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

/// `reply` as the browser gets it. No answer may be cached: each holds a
/// one-time flow or code, and the callback's URL holds the authorization
/// code, which no `Referer` carries on. A page runs only the script its
/// `Content-Security-Policy` names, and is never sniffed as another type.
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
    if let Some(policy) = reply.content_security_policy {
        let policy = HeaderValue::try_from(policy).map_err(|_| {
            ServiceError::backend("the Single Sign-On page's policy is not a header value")
        })?;
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, policy);
    }
    private_reply(&mut response, reply.set_cookies)?;
    Ok(response)
}

/// Marks `response` as one no cache keeps, no `Referer` follows and no
/// browser sniffs, and sets `cookies` on it.
fn private_reply(response: &mut Response, cookies: Vec<String>) -> Result<(), ApiError> {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    for cookie in cookies {
        let cookie = HeaderValue::try_from(cookie).map_err(|_| {
            ServiceError::backend("the Single Sign-On cookie is not a header value")
        })?;
        headers.append(header::SET_COOKIE, cookie);
    }
    Ok(())
}

/// The browser's flow cookie, from every `Cookie` header, under the one name
/// `sso` reads it by ([`SsoManager::flow_cookie_name`]): on an `https` public
/// URL a cookie under the other name, which another host could have planted,
/// is never read.
async fn flow_cookie(
    sso: &dyn SsoManager,
    headers: &HeaderMap,
) -> Result<Option<Secret>, ApiError> {
    if !headers.contains_key(header::COOKIE) {
        return Ok(None);
    }
    let wanted = sso.flow_cookie_name().await?;
    Ok(headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == wanted).then(|| Secret::new(value))
        }))
}

/// `GET /sso/oidc/{provider}/start` — sends the browser to the provider's
/// sign-in page, binding the attempt to it with the flow cookie. Anonymous:
/// this is how a user signs in. With an empty `quickConnect`, the sign-in
/// approves a Quick Connect code typed after it; a `quickConnect` holding
/// anything, a code included, is refused.
#[utoipa::path(
    get,
    path = "/sso/oidc/{provider}/start",
    params(
        ("provider" = String, Path, description = "The provider's key"),
        ("quickConnect" = Option<String>, Query, description = "Present and empty: the sign-in approves a Quick Connect code, typed once it passes")
    ),
    responses(
        (status = 302, description = "To the identity provider, with the flow cookie set"),
        (status = 400, description = "The quickConnect parameter holds a value (a code is typed after the sign-in): a page saying so"),
        (status = 403, description = "With quickConnect, Quick Connect is turned off: a page saying so"),
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
    // Of repeated ones, one that holds something, which the start refuses.
    let quick_connect = form_urlencoded::parse(parts.uri.query().unwrap_or_default().as_bytes())
        .filter(|(name, _)| name == "quickConnect")
        .map(|(_, value)| value.into_owned())
        .min_by_key(String::is_empty);
    browser_reply(match quick_connect {
        Some(value) => {
            sso.start_oidc_quick_connect(&provider, client, &value)
                .await?
        }
        None => sso.start_oidc_sign_in(&provider, client).await?,
    })
}

/// `GET /sso/oidc/{provider}/callback` — where the identity provider sends
/// the browser back. A response that passes every check, for an account found
/// by its link or created, ends on the completion page, which keeps the flow
/// cookie for [`exchange_oidc_sign_in`] (a sign-in started with
/// `quickConnect`: on the confirmation page, which keeps it for
/// [`confirm_quick_connect`]); with no account, on a page saying what to ask
/// an administrator; any other on the refusal page. Both clear the flow
/// cookie. A link flow's callback ends on its own page: the account linked,
/// or refused (the session that started it has ended, a missing required
/// role, another account has the identity, or the account was linked at the
/// issuer since; [`links`]).
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
        (status = 200, description = "The completion page, which signs the verified identity's account in; for a sign-in started with quickConnect, the confirmation page, which asks before approving the code"),
        (status = 400, description = "Refused: the sign-in failed a check, or a link flow's starting session has ended"),
        (status = 403, description = "Refused: no account is linked to the identity and none can be created; the page says what to ask an administrator"),
        (status = 409, description = "A link flow refused: another account is linked to the identity, or the account was linked at the issuer since the flow started"),
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
    let callback = OidcCallback {
        state: once("state"),
        code: once("code").map(Secret::new),
        iss: once("iss"),
        error: once("error"),
        flow_cookie: flow_cookie(sso.as_ref(), &headers).await?,
    };
    browser_reply(sso.finish_oidc_sign_in(&provider, callback).await?)
}

/// `POST /sso/oidc/exchange` — swaps the one-time code of a completion page
/// for a session, once, with the flow cookie the code is bound to, and
/// answers the same `AuthenticationResult` as `POST /Users/AuthenticateByName`.
/// Anonymous: the completion page's script calls it. The session's device is
/// the request's `Authorization: MediaBrowser …` header's, and its address
/// the caller's, as for a password sign-in.
///
/// A refusal is `text/plain`, the sentence the page shows: `400` for a code
/// or cookie that does not check out, `403` for an account the sign-in rules
/// refuse. A session and a refusal both clear the flow cookie; any error
/// answer leaves it, to expire with the code.
#[utoipa::path(
    post,
    path = "/sso/oidc/exchange",
    request_body = OidcExchangeRequest,
    responses(
        (status = 200, description = "Signed in (AuthenticationResult)"),
        (status = 400, description = "Refused: the code is unknown, used, expired, not this browser's or a Quick Connect confirmation, its identity is no longer linked to the account, or the body does not bind"),
        (status = 403, description = "Refused: the account is disabled, outside its schedule or remote access, on a device it may not use, or at its session limit"),
        (status = 404, description = "The Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn exchange_oidc_sign_in(
    State(state): State<AppState>,
    parts: Parts,
    body: Result<JsonBody<OidcExchangeRequest>, ProblemDetails>,
) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    let request = match body {
        Ok(JsonBody(request)) => request,
        Err(problem) => return Ok(problem.into_response()),
    };
    let auth = super::users::auth_info(&parts);
    let session = AuthenticationRequest {
        app: auth.client,
        app_version: auth.version,
        device_id: auth.device_id,
        device_name: auth.device,
        remote_endpoint: Some(state.client_address(&parts).to_string()),
        ..AuthenticationRequest::default()
    };
    let cookie = flow_cookie(sso.as_ref(), &parts.headers).await?;
    let reply = sso
        .exchange_oidc_sign_in(request.code, cookie, session)
        .await?;
    let mut response = match reply.outcome {
        Ok(result) => Json(super::users::authentication_result(result)).into_response(),
        Err(refusal) => {
            let status = StatusCode::from_u16(refusal.status).map_err(|_| {
                ServiceError::backend("the Single Sign-On refusal has no valid status")
            })?;
            (
                status,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                refusal.message,
            )
                .into_response()
        }
    };
    private_reply(&mut response, reply.set_cookies)?;
    Ok(response)
}

/// `GET /sso/quickconnect` — the page that starts a native app's Quick
/// Connect approval: a link for each provider that can sign users in, each
/// starting a sign-in whose confirmation page asks for the code. Anonymous:
/// whoever signs in approves the code they type as themselves.
#[utoipa::path(
    get,
    path = "/sso/quickconnect",
    responses(
        (status = 200, description = "The page with a sign-in link per provider"),
        (status = 403, description = "Quick Connect is turned off: a page saying so"),
        (status = 404, description = "No provider can sign users in, or the Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn quick_connect_page(State(state): State<AppState>) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    browser_reply(sso.quick_connect_page().await?)
}

/// `POST /sso/quickconnect` — the confirmation page's form: approves the
/// Quick Connect `code` typed on it, once, with the flow cookie its
/// `confirmation` is bound to, as the account the sign-in found, from the
/// caller's address. Answers a page either way, and clears the flow cookie.
#[utoipa::path(
    post,
    path = "/sso/quickconnect",
    request_body(content = String, content_type = "application/x-www-form-urlencoded", description = "confirmation=…&code=…, from the confirmation page"),
    responses(
        (status = 200, description = "The app is signed in: a page saying so"),
        (status = 400, description = "Refused: the confirmation is unknown, used, expired or not this browser's, its identity is no longer linked to the account, or the code is not one, or is unknown, expired or approved already"),
        (status = 403, description = "Refused: Quick Connect is turned off, or a sign-in rule refuses the account"),
        (status = 404, description = "The Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn confirm_quick_connect(
    State(state): State<AppState>,
    parts: Parts,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    let form: Vec<(String, String)> = form_urlencoded::parse(&body).into_owned().collect();
    let field = |wanted: &str| {
        form.iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, value)| value.as_str())
    };
    let confirmation = field("confirmation").map(Secret::new);
    let cookie = flow_cookie(sso.as_ref(), &parts.headers).await?;
    let client = state.client_address(&parts);
    browser_reply(
        sso.confirm_oidc_quick_connect(confirmation, field("code"), cookie, client)
            .await?,
    )
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
        .route("/sso/oidc/exchange", post(exchange_oidc_sign_in))
        .route(
            "/sso/quickconnect",
            get(quick_connect_page).post(confirm_quick_connect),
        )
        .merge(links::register(Router::new()))
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
            content_security_policy: None,
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
