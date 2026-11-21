//! The account-link routes of the Single Sign-On plugin (B12):
//! - `GET /sso/link` — the link page, for anyone: its script reads the
//!   session jellyfin-web keeps in the browser and calls the routes below;
//! - `GET /sso/links` — the caller's own links and the providers to link at;
//! - `POST /sso/oidc/{provider}/link` — starts a link of the caller's account
//!   at the provider, with the account's password, answering the URL to send
//!   the browser to (a top-level navigation cannot carry the access token, so
//!   the flow starts here);
//! - `DELETE /sso/links/{provider}` — removes the caller's link there;
//! - `GET`, `POST` and `DELETE /sso/admin/links` — every link, for an
//!   administrator, who adds one by hand and removes any.
//!
//! A user's route acts on the account its credentials name, never one a
//! parameter names: only the admin routes take a user id. Removing a link
//! signs every session of its account out.

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{
    OidcAdminLink, OidcLinkRequest, OidcLinkStart, OidcLinkStartRequest, OidcUserLinks,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::sso::OidcLinkAt;
use uuid::Uuid;

use super::{browser_reply, enabled_service, private_reply, sign_in_provider};
use crate::auth::{RequireAdmin, RequireAuth};
use crate::error::ApiError;
use crate::extract::{JsonBody, ProblemDetails, Query, QueryParameters, QueryRejection};
use crate::state::AppState;

/// The account a user's route acts for, the one the credentials name, and
/// the session's access token. An API key names neither.
fn caller_session(auth: Result<RequireAuth, ApiError>) -> Result<(Uuid, Secret), ApiError> {
    let RequireAuth(info) = auth?;
    let user = info.user_id();
    match info.token {
        Some(token) if !user.is_nil() => Ok((user, token)),
        _ => Err(ApiError::Forbidden(
            "linking needs a user's session, not an API key".to_owned(),
        )),
    }
}

/// The account a user's route acts for ([`caller_session`]).
fn caller(auth: Result<RequireAuth, ApiError>) -> Result<Uuid, ApiError> {
    caller_session(auth).map(|(user, _)| user)
}

/// `GET /sso/link` — the link page. Anonymous as a document: without a
/// session in the browser, it says to sign in first.
#[utoipa::path(
    get,
    path = "/sso/link",
    responses(
        (status = 200, description = "The link page, which may run its own script only"),
        (status = 404, description = "The Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn link_page(State(state): State<AppState>) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    browser_reply(sso.oidc_link_page().await?)
}

/// `GET /sso/links` — the caller's links (provider key and name, issuer and
/// date, never a subject), the enabled providers to link at, and whether the
/// account signs in through Single Sign-On only.
#[utoipa::path(
    get,
    path = "/sso/links",
    responses(
        (status = 200, description = "The caller's links", body = OidcUserLinks),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "An API key, or an account the access policy refuses"),
        (status = 404, description = "The Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn my_links(
    State(state): State<AppState>,
    auth: Result<RequireAuth, ApiError>,
) -> Result<Json<OidcUserLinks>, ApiError> {
    let sso = enabled_service(&state).await?;
    let user = caller(auth)?;
    Ok(Json(sso.oidc_links(user).await?))
}

/// `POST /sso/oidc/{provider}/link` — starts a link of the caller's account
/// at the provider, once the body's `Password` signs the account in: sets the
/// flow cookie and answers the identity provider's sign-in URL, for the link
/// page to send the browser to. Its callback links the verified identity to
/// this account while the caller's session lasts, unless another account has
/// it or this one was linked at that issuer since, and opens no session. A
/// refusal sets no cookie.
#[utoipa::path(
    post,
    path = "/sso/oidc/{provider}/link",
    params(("provider" = String, Path, description = "The provider's key")),
    request_body = OidcLinkStartRequest,
    responses(
        (status = 200, description = "Where to send the browser, with the flow cookie set", body = OidcLinkStart),
        (status = 400, description = "No password, or the body does not bind"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "A wrong password (counted and paced as at sign-in), an account that signs in through Single Sign-On only (an administrator links it), an API key, or an account the access policy refuses"),
        (status = 404, description = "No such enabled provider, or the Single Sign-On plugin is disabled or not available"),
        (status = 409, description = "The account is linked at the provider's issuer already: that link is removed first"),
        (status = 502, description = "The identity provider could not be reached: a page saying so"),
        (status = 503, description = "No public URL is set to build the callback URL on: a page naming the setting")
    ),
    tag = "ferrofin"
)]
async fn start_link(
    State(state): State<AppState>,
    auth: Result<RequireAuth, ApiError>,
    provider: Result<Path<String>, PathRejection>,
    parts: Parts,
    body: Result<JsonBody<OidcLinkStartRequest>, ProblemDetails>,
) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    let (user, session) = caller_session(auth)?;
    let provider = sign_in_provider(provider)?;
    let request = match body {
        Ok(JsonBody(request)) => request,
        Err(problem) => return Ok(problem.into_response()),
    };
    let mut reply = sso
        .start_oidc_link(
            &provider,
            user,
            &request.password,
            &session,
            state.client_address(&parts),
        )
        .await?;
    let Some(url) = reply.location.take() else {
        // Not started: the page that says why.
        return browser_reply(reply);
    };
    let mut response = Json(OidcLinkStart { url }).into_response();
    private_reply(&mut response, reply.set_cookies)?;
    Ok(response)
}

/// `DELETE /sso/links/{provider}` — removes the caller's link at the
/// provider, then signs every session of the account out, the caller's
/// included.
#[utoipa::path(
    delete,
    path = "/sso/links/{provider}",
    params(("provider" = String, Path, description = "The provider's key")),
    responses(
        (status = 204, description = "The link was removed and the account's sessions signed out"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "An API key, or an account the access policy refuses"),
        (status = 404, description = "No such provider or link, or the Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn remove_my_link(
    State(state): State<AppState>,
    auth: Result<RequireAuth, ApiError>,
    provider: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    let user = caller(auth)?;
    let provider = sign_in_provider(provider)?;
    sso.remove_oidc_link(user, user, OidcLinkAt::Provider(&provider))
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /sso/admin/links` — every link, with its account's id and name, its
/// provider, issuer, subject and date.
#[utoipa::path(
    get,
    path = "/sso/admin/links",
    responses(
        (status = 200, description = "Every link", body = [OidcAdminLink]),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "The Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn all_links(
    State(state): State<AppState>,
    admin: Result<RequireAdmin, ApiError>,
) -> Result<Json<Vec<OidcAdminLink>>, ApiError> {
    let sso = enabled_service(&state).await?;
    admin?;
    Ok(Json(sso.all_oidc_links().await?))
}

/// `POST /sso/admin/links` — links an account to a subject at a provider,
/// whose issuer it takes, replacing the account's earlier link at that
/// issuer. Linking never changes the account's authentication provider or
/// password.
#[utoipa::path(
    post,
    path = "/sso/admin/links",
    request_body = OidcLinkRequest,
    responses(
        (status = 200, description = "The link", body = OidcAdminLink),
        (status = 400, description = "The subject is refused, or the body does not bind"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "No such account or provider, or the Single Sign-On plugin is disabled or not available"),
        (status = 409, description = "Another account is linked to the identity: nothing was moved")
    ),
    tag = "ferrofin"
)]
async fn add_link(
    State(state): State<AppState>,
    admin: Result<RequireAdmin, ApiError>,
    body: Result<JsonBody<OidcLinkRequest>, ProblemDetails>,
) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    let RequireAdmin(admin) = admin?;
    let request = match body {
        Ok(JsonBody(request)) => request,
        Err(problem) => return Ok(problem.into_response()),
    };
    let link = sso
        .add_oidc_link(
            admin.user_id(),
            request.user_id,
            &request.provider,
            &request.subject,
        )
        .await?;
    Ok(Json(link).into_response())
}

/// The query of `DELETE /sso/admin/links`: the account, and the link's
/// provider key or, for a link no provider has any more, its issuer.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemovalQuery {
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    issuer: Option<String>,
}

impl QueryParameters for RemovalQuery {
    const COLLECTIONS: &'static [(&'static str, char)] = &[];
}

/// `DELETE /sso/admin/links?userId=…&provider=…` (or `&issuer=…`) — removes
/// one link of the account, then signs every session of that account out.
#[utoipa::path(
    delete,
    path = "/sso/admin/links",
    params(
        ("userId" = String, Query, description = "The account"),
        ("provider" = Option<String>, Query, description = "The provider's key"),
        ("issuer" = Option<String>, Query, description = "Instead of the provider: the link's issuer")
    ),
    responses(
        (status = 204, description = "The link was removed and the account's sessions signed out"),
        (status = 400, description = "No account, or not exactly one of the provider and the issuer"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "No such provider or link, or the Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn remove_link(
    State(state): State<AppState>,
    admin: Result<RequireAdmin, ApiError>,
    query: Result<Query<RemovalQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    let RequireAdmin(admin) = admin?;
    let query = match query {
        Ok(Query(query)) => query,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    let user = query
        .user_id
        .ok_or_else(|| ServiceError::invalid_input("userId is required"))?;
    let at = match (query.provider.as_deref(), query.issuer.as_deref()) {
        (Some(provider), None) => OidcLinkAt::Provider(provider),
        (None, Some(issuer)) => OidcLinkAt::Issuer(issuer),
        _ => {
            return Err(
                ServiceError::invalid_input("name the link by its provider or its issuer").into(),
            );
        }
    };
    sso.remove_oidc_link(admin.user_id(), user, at).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Registers the link routes onto `router`.
pub(super) fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/sso/link", get(link_page))
        .route("/sso/links", get(my_links))
        .route(
            "/sso/links/{provider}",
            axum::routing::delete(remove_my_link),
        )
        .route("/sso/oidc/{provider}/link", post(start_link))
        .route(
            "/sso/admin/links",
            get(all_links).post(add_link).delete(remove_link),
        )
}
