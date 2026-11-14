//! Mount the API/web routes under the advertised base URL.
//!
//! Mirrors Jellyfin's BaseUrlRedirectionMiddleware followed by Startup's Map:
//! Jellyfin's `/health` outside the mount redirects into it. Ferrofin's own
//! probe endpoints (`/health/live`, `/health/ready`) also remain available at
//! the root so deployments can keep probing while the base URL changes.
//!
//! [`resolve`] picks the base URL a server lifetime serves under, and
//! [`restart_listener`] marks a restart pending when a Networking page save
//! changes it.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ferrofin_core::system_manager::LifecycleController;
use ferrofin_networking::NetworkConfiguration;
use ferrofin_traits::configuration::ConfigurationListener;

pub(crate) use ferrofin_networking::normalize_base_url as normalize;

/// The base URL a server lifetime mounts, redirects to and advertises: the
/// `FERROFIN_BASE_URL` variable when `variable_set`, else the Networking
/// page's `BaseUrl` saved in `network` when it is not empty, else
/// `config.toml`'s `base_url`, else none (owner decision on gl-555).
/// `bootstrap` is the loader's pick of the variable and the file.
///
/// Upstream mounts `BaseUrl` once per host (`Startup.Configure`'s `app.Map`),
/// while its redirect middleware and `GetLocalApiUrl` read the saved value
/// live. Here the one value serves the whole lifetime, so a request never
/// meets two prefixes; a changed one waits for the restart.
pub(crate) fn resolve(
    bootstrap: &str,
    variable_set: bool,
    network: &NetworkConfiguration,
) -> String {
    let saved = normalize(&network.base_url);
    if variable_set || saved.is_empty() {
        normalize(bootstrap)
    } else {
        saved
    }
}

/// Marks a restart pending in `lifecycle` when a saved network configuration
/// resolves ([`resolve`]) to another base URL than `running`, the one this
/// lifetime serves under. Upstream's `OnConfigurationUpdated` flags no restart
/// for `BaseUrl`, though its mount keeps the old value until one; the owner's
/// decision on gl-555 is that the dashboard says so.
pub(crate) fn restart_listener(
    running: String,
    bootstrap: String,
    variable_set: bool,
    lifecycle: Arc<dyn LifecycleController>,
) -> Arc<ConfigurationListener> {
    crate::state::network_restart_listener(
        running,
        move |saved| resolve(&bootstrap, variable_set, saved),
        "serve under the new base URL",
        lifecycle,
    )
}

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
    use ferrofin_traits::configuration::ConfigurationUpdate;
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

    fn saved(base_url: &str) -> NetworkConfiguration {
        NetworkConfiguration {
            base_url: base_url.to_owned(),
            ..NetworkConfiguration::default()
        }
    }

    /// gl-555: the variable, else the Networking page, else `config.toml`,
    /// else none. A blank variable is set (to the root), as the loader reads it.
    #[rstest::rstest]
    #[case::variable_outranks_the_page("/env", true, "/page", "/env")]
    #[case::blank_variable_outranks_the_page("", true, "/page", "")]
    #[case::page_outranks_the_file("/file", false, "page/", "/page")]
    #[case::page_alone("", false, "/page", "/page")]
    #[case::empty_page_leaves_the_file("/file", false, "", "/file")]
    #[case::slash_page_leaves_the_file("/file/", false, "/", "/file")]
    #[case::none("", false, "", "")]
    fn resolve_follows_the_owners_precedence(
        #[case] bootstrap: &str,
        #[case] variable_set: bool,
        #[case] page: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(resolve(bootstrap, variable_set, &saved(page)), expected);
    }

    /// A Networking save marks a restart only when it changes the base URL
    /// served, and the variable keeps it from changing.
    #[test]
    fn a_saved_base_url_change_marks_a_restart() {
        let save = |running: &str, variable_set: bool, json: &str| {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            let lifecycle = Arc::new(crate::state::FerrofinLifecycleController::new(tx));
            let listener = restart_listener(
                running.to_owned(),
                "/file".to_owned(),
                variable_set,
                lifecycle.clone(),
            );
            listener(&ConfigurationUpdate::Named {
                key: "network".to_owned(),
                json: json.into(),
            });
            lifecycle.has_pending_restart()
        };
        assert!(save("/file", false, r#"{"BaseUrl":"/jellyfin"}"#));
        assert!(save("/jellyfin", false, r#"{"BaseUrl":""}"#));
        assert!(
            save("/jellyfin", false, "not json"),
            "boots as the defaults"
        );
        assert!(!save("/jellyfin", false, r#"{"BaseUrl":"jellyfin/"}"#));
        assert!(!save("/file", false, r#"{"BaseUrl":""}"#));
        assert!(!save("/file", true, r#"{"BaseUrl":"/jellyfin"}"#));

        let (tx, _rx) = tokio::sync::oneshot::channel();
        let lifecycle = Arc::new(crate::state::FerrofinLifecycleController::new(tx));
        let listener = restart_listener(String::new(), String::new(), false, lifecycle.clone());
        listener(&ConfigurationUpdate::Named {
            key: "encoding".to_owned(),
            json: r#"{"BaseUrl":"/jellyfin"}"#.into(),
        });
        assert!(!lifecycle.has_pending_restart(), "another document");
    }
}
