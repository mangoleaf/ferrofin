//! [`create_router`] — assembles the full `hermit-api` [`axum::Router`].
//!
//! Every path+method in the vendored Jellyfin contract is registered so a known
//! route never 404s. Unit 1 (INFRA) points them all at the shared
//! `not_implemented` stub (`501`); later waves register real handlers over the
//! matching entries. The router also mounts the health probes, the OpenAPI spec,
//! the auth-context middleware, and permissive CORS + tracing.

use axum::extract::{MatchedPath, Request};
use axum::http::uri::{PathAndQuery, Uri};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{MethodRouter, delete, get, head, post};
use axum::{Router, middleware};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::Span;

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
    .layer(middleware::from_fn(merge_repeated_query_params))
    // Route-templated per-request span (traces group by endpoint, not raw URL);
    // a sampled request's `trace_id` is stamped on for log↔trace correlation.
    // With OTLP export off the OTel context is invalid, so no `trace_id` is
    // recorded and the layer degrades to plain per-request spans as before.
    .layer(
        TraceLayer::new_for_http()
            .make_span_with(make_request_span)
            .on_request(record_trace_id)
            .on_response(record_status),
    )
    .layer(CorsLayer::permissive())
}

/// Builds the per-request span, named by the matched route template so
/// `tracing-opentelemetry` groups spans by endpoint (via `otel.name`).
fn make_request_span(req: &Request) -> Span {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str);
    tracing::info_span!(
        "http_request",
        otel.name = %format!("{} {route}", req.method()),
        http.request.method = %req.method(),
        http.route = route,
        url.path = %req.uri().path(),
        http.response.status_code = tracing::field::Empty,
        trace_id = tracing::field::Empty,
    )
}

/// Stamps the sampled trace id onto the request span. Every `tracing` event
/// inside the request inherits the field, so logs join to their span with no
/// per-callsite work. Unsampled/unexported requests get no field.
fn record_trace_id(_req: &Request, span: &Span) {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    let ctx = span.context();
    let span_ctx = ctx.span().span_context().clone();
    if span_ctx.is_valid() && span_ctx.is_sampled() {
        span.record("trace_id", span_ctx.trace_id().to_string());
    }
}

/// Records the final HTTP status code onto the request span.
fn record_status(res: &Response, _latency: std::time::Duration, span: &Span) {
    span.record("http.response.status_code", res.status().as_u16());
}

/// Rewrites `?a=1&a=2` to `?a=1,2` before the typed `Query` extractors run.
///
/// The jellyfin SDK serializes every array-valued query parameter as a
/// **repeated** key (`?includeItemTypes=Movie&includeItemTypes=Series`), while
/// the handlers' typed query structs model them as comma-delimited strings —
/// serde rejects the repeated form as a duplicate field with `400`, which broke
/// e.g. the whole web search page. ASP.NET's collection binder accepts both
/// forms, so merging duplicates into the comma form (operating on the still
/// percent-encoded raw query) restores parity for every route at once.
async fn merge_repeated_query_params(mut request: Request, next: Next) -> Response {
    if let Some(merged) = request.uri().query().and_then(merged_query)
        && let Ok(path_and_query) = PathAndQuery::try_from(match merged.as_str() {
            "" => request.uri().path().to_owned(),
            q => format!("{}?{q}", request.uri().path()),
        })
    {
        let mut parts = request.uri().clone().into_parts();
        parts.path_and_query = Some(path_and_query);
        if let Ok(uri) = Uri::from_parts(parts) {
            *request.uri_mut() = uri;
        }
    }
    next.run(request).await
}

/// Merges repeated query keys into single comma-joined values, preserving
/// first-occurrence order and percent-encoding. Returns `None` when no key
/// repeats (the common case — the URI is left untouched).
fn merged_query(query: &str) -> Option<String> {
    // (key, values, saw_equals) per distinct key, in first-seen order. A pair
    // without `=` (a bare flag) keeps its bare form on rebuild.
    let mut groups: Vec<(&str, Vec<&str>, bool)> = Vec::new();
    let mut has_duplicate = false;
    for pair in query.split('&') {
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (pair, None),
        };
        match groups.iter_mut().find(|(k, ..)| *k == key) {
            Some((_, values, saw_equals)) => {
                has_duplicate = true;
                values.extend(value);
                *saw_equals |= value.is_some();
            }
            None => groups.push((key, value.into_iter().collect(), value.is_some())),
        }
    }
    has_duplicate.then(|| {
        groups
            .iter()
            .map(|(key, values, saw_equals)| {
                if *saw_equals {
                    format!("{key}={}", values.join(","))
                } else {
                    (*key).to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("&")
    })
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
    use super::{create_router, merged_query};
    use crate::routes::axum_routes;
    use crate::test_support::fake_state;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn merged_query_joins_repeated_keys_in_order() {
        assert_eq!(
            merged_query("a=1&b=x&a=2&a=3").as_deref(),
            Some("a=1,2,3&b=x")
        );
        // Percent-encoded values are joined verbatim (no decode/re-encode).
        assert_eq!(
            merged_query("fields=A%2CB&fields=C").as_deref(),
            Some("fields=A%2CB,C")
        );
        // Bare flags and empty values survive.
        assert_eq!(merged_query("flag&a=1&a=").as_deref(), Some("flag&a=1,"));
    }

    #[test]
    fn merged_query_leaves_unique_keys_alone() {
        assert_eq!(merged_query("a=1&b=2"), None);
        assert_eq!(merged_query(""), None);
        assert_eq!(merged_query("searchTerm=a,b"), None);
    }

    #[tokio::test]
    async fn repeated_array_params_do_not_400() {
        // The jellyfin SDK repeats array params (`?fields=A&fields=B`); the
        // typed Query extractors must not reject them as duplicate fields.
        // (This request 401s at auth — the point is it gets past query
        // deserialization, which used to 400 first.)
        let router = create_router(fake_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/Items?includeItemTypes=Movie&includeItemTypes=Series&fields=CanDelete&fields=MediaSourceCount")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

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

    #[test]
    fn request_span_gets_a_valid_sampled_trace_id_under_otel() {
        // With an OTel layer active, the request span carries a valid, sampled
        // context — the exact precondition `record_trace_id` keys on before
        // stamping the log↔trace correlation field. Guards the correlation wiring
        // against a version bump silently breaking context assignment.
        use super::make_request_span;
        use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
        use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider};
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        let provider = SdkTracerProvider::builder()
            .with_sampler(Sampler::AlwaysOn)
            .with_simple_exporter(InMemorySpanExporter::default())
            .build();
        let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("hermit"));
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            let req = Request::builder()
                .uri("/Items")
                .body(Body::empty())
                .unwrap();
            let span = make_request_span(&req);
            let _entered = span.enter();
            let span_ctx = span.context().span().span_context().clone();
            assert!(span_ctx.is_valid(), "otel context assigned to the span");
            assert!(span_ctx.is_sampled(), "AlwaysOn sampler → sampled");
            assert_eq!(
                span_ctx.trace_id().to_string().len(),
                32,
                "trace id renders as 32 hex chars"
            );
        });
    }

    #[test]
    fn request_span_has_no_valid_trace_id_without_otel() {
        // No OTel layer ⇒ invalid context ⇒ `record_trace_id` no-ops (no dead
        // Tempo links when export is off).
        use super::make_request_span;
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;

        let req = Request::builder()
            .uri("/Items")
            .body(Body::empty())
            .unwrap();
        let span = make_request_span(&req);
        let _entered = span.enter();
        assert!(!span.context().span().span_context().is_valid());
    }
}
