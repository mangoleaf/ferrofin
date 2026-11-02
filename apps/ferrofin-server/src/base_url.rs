//! Mount the API/web routes under the advertised base URL.
//!
//! Mirrors Jellyfin's BaseUrlRedirectionMiddleware followed by Startup's Map:
//! Jellyfin's `/health` outside the mount redirects into it. Ferrofin's own
//! probe endpoints (`/health/live`, `/health/ready`) also remain available at
//! the root so deployments can keep probing while the base URL changes.

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub(crate) use ferrofin_networking::normalize_base_url as normalize;

/// Strip the mount before case canonicalization/routing, and retain it in redirects.
pub(crate) async fn rewrite(
    State(base): State<String>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if base.is_empty()
        || path.eq_ignore_ascii_case("/health/live")
        || path.eq_ignore_ascii_case("/health/ready")
    {
        return next.run(request).await;
    }

    let has_prefix = path
        .get(..base.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&base));
    let suffix = path.get(base.len()..).unwrap_or_default();
    if !has_prefix || suffix.is_empty() || suffix == "/" {
        // Upstream sends a health check outside the mount into it.
        if path.eq_ignore_ascii_case("/health") || path.eq_ignore_ascii_case("/health/") {
            return (
                StatusCode::FOUND,
                [(header::LOCATION, format!("{base}/health"))],
            )
                .into_response();
        }
        // Jellyfin redirects missing prefixes and the mount root to its default
        // web path. Use 302 (not Axum Redirect's 303/307/308).
        return (
            StatusCode::FOUND,
            [(header::LOCATION, format!("{base}/web/"))],
        )
            .into_response();
    }
    if !suffix.starts_with('/') {
        // StartsWith in upstream's redirect middleware succeeds, but Map rejects
        // a non-segment prefix (e.g. /jellyfin-other for /jellyfin).
        return StatusCode::NOT_FOUND.into_response();
    }

    let path_and_query = request
        .uri()
        .query()
        .map_or_else(|| suffix.to_owned(), |query| format!("{suffix}?{query}"));
    let mut parts = request.uri().clone().into_parts();
    let Ok(path_and_query) = path_and_query.parse() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    parts.path_and_query = Some(path_and_query);
    let Ok(uri) = Uri::from_parts(parts) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    *request.uri_mut() = uri;

    let mut response = next.run(request).await;
    // Existing API/web handlers produce application-root paths. Absolute URLs,
    // network-path references and relative paths already retain their meaning.
    if let Some(location) = response.headers().get(header::LOCATION)
        && let Ok(location) = location.to_str()
        && location.starts_with('/')
        && !location.starts_with("//")
        && let Ok(location) = HeaderValue::from_str(&format!("{base}{location}"))
    {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::routing::get;
    use tower::{Layer as _, ServiceExt as _};

    use super::*;

    async fn send(base: &str, path: &str) -> Response {
        let router = Router::new()
            .route(
                "/System/Info/Public",
                get(|uri: Uri| async move { uri.to_string() }),
            )
            .route("/health", get(|| async { "health" }))
            .route("/health/live", get(|| async { "live" }))
            .route("/health/ready", get(|| async { "ready" }))
            .route(
                "/web",
                get(|| async { (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, "/web/")]) }),
            )
            .route(
                "/absolute",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, "https://example.org/")],
                    )
                }),
            )
            .route(
                "/network",
                get(|| async { (StatusCode::FOUND, [(header::LOCATION, "//example.org/")]) }),
            );
        let app = axum::middleware::from_fn(crate::canonicalize_path_case).layer(router);
        let app = axum::middleware::from_fn_with_state(normalize(base), rewrite).layer(app);
        app.oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("infallible router")
    }

    #[test]
    fn normalization_handles_missing_and_extra_slashes() {
        for (input, expected) in [
            ("", ""),
            ("/", ""),
            ("   ", ""),
            ("jellyfin", "/jellyfin"),
            ("/jellyfin/", "/jellyfin"),
            ("/jellyfin///", "/jellyfin"),
            ("/media/jellyfin/", "/media/jellyfin"),
        ] {
            assert_eq!(normalize(input), expected);
        }
    }

    #[tokio::test]
    async fn prefix_is_stripped_before_case_routing_and_query_is_preserved() {
        let response = send("jellyfin/", "/JeLlYfIn/system/info/public?key=Some%2FValue").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(body, "/System/Info/Public?key=Some%2FValue");
        assert_eq!(
            send("/jellyfin///", "/jellyfin/System/Info/Public")
                .await
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn missing_prefix_and_bare_mount_redirect_to_web() {
        for path in ["/", "/System/Info/Public", "/jellyfin", "/JELLYFIN/"] {
            let response = send("/jellyfin", path).await;
            assert_eq!(response.status(), StatusCode::FOUND, "{path}");
            assert_eq!(response.headers()[header::LOCATION], "/jellyfin/web/");
        }
        assert_eq!(
            send("/jellyfin", "/jellyfin-other").await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn redirects_keep_mount_without_changing_external_locations() {
        for (path, location) in [
            ("/jellyfin/web", "/jellyfin/web/"),
            ("/jellyfin/absolute", "https://example.org/"),
            ("/jellyfin/network", "//example.org/"),
        ] {
            assert_eq!(
                send("/jellyfin", path).await.headers()[header::LOCATION],
                location
            );
        }
    }

    #[tokio::test]
    async fn health_probes_work_at_root_and_under_mount() {
        for path in [
            "/health/live",
            "/health/ready",
            "/jellyfin/health/live",
            "/jellyfin/health/ready",
            "/jellyfin/health",
            "/JELLYFIN/HEALTH",
        ] {
            assert_eq!(
                send("/jellyfin", path).await.status(),
                StatusCode::OK,
                "{path}"
            );
        }
    }

    /// Upstream `BaseUrlRedirectionMiddleware`: `/health` (or `/health/`)
    /// outside the mount redirects to `{base}/health`, not to the web client.
    #[tokio::test]
    async fn bare_health_redirects_into_the_mount() {
        for path in ["/health", "/health/", "/HEALTH"] {
            let response = send("/jellyfin", path).await;
            assert_eq!(response.status(), StatusCode::FOUND, "{path}");
            assert_eq!(response.headers()[header::LOCATION], "/jellyfin/health");
        }
    }

    #[tokio::test]
    async fn empty_base_preserves_root_routes() {
        assert_eq!(
            send("", "/System/Info/Public").await.status(),
            StatusCode::OK
        );
        assert_eq!(send("/", "/web").await.headers()[header::LOCATION], "/web/");
        assert_eq!(send("", "/HEALTH").await.status(), StatusCode::OK);
    }
}
