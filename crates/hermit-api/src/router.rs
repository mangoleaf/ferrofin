//! [`create_router`] — assembles the full `hermit-api` [`axum::Router`].
//!
//! Every path+method in the vendored Jellyfin contract is registered so a known
//! route never 404s. Unit 1 (INFRA) points them all at the shared
//! `not_implemented` stub (`501`); later waves register real handlers over the
//! matching entries. The router also mounts the health probes, the OpenAPI spec,
//! the auth-context middleware, and permissive CORS + tracing.

use axum::routing::{MethodRouter, delete, get, head, post};
use axum::{Router, middleware};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::auth::auth_context_layer;
use crate::handlers;
use crate::openapi::ApiDoc;
use crate::routes::{axum_routes, not_implemented};
use crate::state::AppState;

/// Builds the shared `not_implemented` [`MethodRouter`] for one HTTP method.
///
/// Returns `None` for a method the contract never uses, so an unexpected verb in
/// the vendored table surfaces as a build/test failure rather than a silent drop.
fn stub_for(method: &str) -> Option<MethodRouter<AppState>> {
    Some(match method {
        "get" => get(not_implemented),
        "post" => post(not_implemented),
        "delete" => delete(not_implemented),
        "head" => head(not_implemented),
        _ => return None,
    })
}

/// Assembles the complete `hermit-api` router over the injected [`AppState`].
///
/// Registers all vendored contract routes (as `501` stubs in this unit), merges
/// the health-probe router and the OpenAPI spec, and layers the auth-context
/// middleware, CORS, and HTTP tracing.
///
/// # Panics
///
/// Panics if the vendored route table names an HTTP method other than
/// `get`/`post`/`delete`/`head`, or if a normalized route fails to register —
/// both are contract/programming errors caught by the crate's tests, never by
/// runtime input.
pub fn create_router(state: AppState) -> Router {
    let mut api: Router<AppState> = Router::new();
    for (method, path) in axum_routes() {
        // Skip the shared `501` stub for any `(method, path)` a real handler
        // covers; axum panics if a method+path is registered twice, so the real
        // handler must be the sole route for it.
        if handlers::REAL_ROUTES
            .iter()
            .any(|(m, p)| *m == method && *p == path)
        {
            continue;
        }
        let stub = stub_for(method)
            .unwrap_or_else(|| panic!("vendored contract uses unsupported method {method:?}"));
        api = api.route(&path, stub);
    }
    // Mount the real First-Light handlers over the (now-skipped) stub slots.
    api = handlers::register(api);

    api.layer(middleware::from_fn_with_state(
        state.clone(),
        auth_context_layer,
    ))
    .with_state(state)
    .merge(hermit_health::health_router(Vec::new()))
    .merge(spec_router())
    .layer(TraceLayer::new_for_http())
    .layer(CorsLayer::permissive())
}

/// A tiny router serving the merged OpenAPI document at
/// `/api-docs/openapi.json`.
///
/// The document is `hermit-api`'s own [`ApiDoc`] with the shared health paths
/// merged in, so `/health/live` + `/health/ready` appear alongside the ported
/// endpoints.
fn spec_router() -> Router {
    use utoipa::OpenApi;

    let mut doc = ApiDoc::openapi();
    doc.merge(hermit_health::HealthApi::openapi());
    Router::new().route(
        "/api-docs/openapi.json",
        get(move || {
            let doc = doc.clone();
            async move { axum::Json(doc) }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::create_router;
    use crate::routes::axum_routes;
    use crate::test_support::fake_state;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn stubbed_route_returns_501_not_404() {
        // Every vendored contract route now has a real handler, so the shared
        // stub no longer fronts any live route. The stub *mechanism* still
        // guards any future un-ported contract path — it must map to `501`
        // (not `404`), so exercise `not_implemented` directly.
        use crate::routes::not_implemented;
        use axum::routing::get;
        let router: axum::Router = axum::Router::new().route("/_unported", get(not_implemented));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/_unported")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn real_authenticated_route_returns_401_without_token() {
        // `/System/Info` now has a real handler behind `RequireAuth`; the fake
        // auth service rejects the tokenless request, so it is `401` (route
        // exists) rather than the `501` stub or a `404`.
        let router = create_router(fake_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/System/Info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_route_returns_404() {
        let router = create_router(fake_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/definitely/not/a/real/route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn health_and_spec_are_mounted() {
        let router = create_router(fake_state());
        let live = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(live.status(), StatusCode::OK);

        let spec = router
            .oneshot(
                Request::builder()
                    .uri("/api-docs/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(spec.status(), StatusCode::OK);
    }

    #[test]
    fn every_normalized_route_registers() {
        // Building the router registers all routes; a bad pattern would panic
        // here. Also assert we actually registered the full table.
        let _ = create_router(fake_state());
        assert!(axum_routes().len() >= 400);
    }
}
