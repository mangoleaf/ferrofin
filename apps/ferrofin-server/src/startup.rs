//! The startup server — port of `Jellyfin.Server.ServerSetupApp.SetupServer`.
//!
//! A server lifetime binds its listeners before it adopts or migrates the
//! database (a scheduled backup restore runs first, because it may replace
//! `network.xml`), and answers on them at once: until the lifetime's
//! router is ready, every request gets the setup answers below; afterwards it
//! goes to that router. The handover happens on the same sockets, so there is
//! no window in which the port is closed (upstream stops the setup host and
//! starts the real one).
//!
//! While starting:
//! - the probes come from [`ferrofin_health::startup_router`]: `/health` is
//!   `Degraded` (200), `/health/live` 200, `/health/ready` 503;
//! - `GET /System/Info/Public` waits up to [`PUBLIC_INFO_RETRIES`] ×
//!   [`PUBLIC_INFO_RETRY_DELAY`] for the router, as upstream polls for its
//!   application host, and answers from it once ready;
//! - everything else is `503 Service Unavailable` with `Retry-After: 005` and
//!   a short HTML page.
//!
//! The probe paths are matched as written and outside the base URL, where
//! probes send them; the lifetime's router canonicalizes case and the base
//! URL once it serves.
//!
//! TODO(gl-865): upstream's page renders the current startup activity and the
//! startup log (`StartupUiRenderer`, `index.mstemplate.html`), serves the
//! latest log file at `/startup/logger` to the local network, and keeps the
//! setup server up in a reporting mode when startup fails. Port them onto
//! [`dispatcher`]: an activity cell set at upstream's `ReportActivity` points,
//! a startup log queue fed by a `tracing` layer, the template, the logger
//! route behind the early network manager's local-network check, and
//! reporting mode in `serve_once`'s error path.

use std::convert::Infallible;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tower::ServiceExt as _;

/// The router a lifetime serves once its state is wired; empty while it
/// starts.
pub(crate) type Ready = Arc<OnceLock<Router>>;

/// How long a starting server asks clients to wait: upstream's
/// `retryAfterValue`, written as `TotalSeconds.ToString("000")`.
const RETRY_AFTER: &str = "005";

/// How many times `GET /System/Info/Public` waits for the router (upstream's
/// `retryCounter < 5`).
const PUBLIC_INFO_RETRIES: u32 = 5;

/// The wait between two of those checks (upstream's `Task.Delay(500)`).
const PUBLIC_INFO_RETRY_DELAY: Duration = Duration::from_millis(500);

/// The service a lifetime's listeners serve: the setup answers until `ready`
/// holds a router, then that router.
pub(crate) fn dispatcher(ready: Ready) -> Router {
    let setup = ferrofin_health::startup_router().fallback(starting);
    Router::new().fallback_service(tower::service_fn(move |request: Request| {
        let (ready, setup) = (Arc::clone(&ready), setup.clone());
        async move {
            if let Some(app) = ready.get() {
                return app.clone().oneshot(request).await;
            }
            if is_public_system_info(&request) {
                for _ in 0..PUBLIC_INFO_RETRIES {
                    tokio::time::sleep(PUBLIC_INFO_RETRY_DELAY).await;
                    if let Some(app) = ready.get() {
                        return app.clone().oneshot(request).await;
                    }
                }
                return Ok::<_, Infallible>(unavailable(None));
            }
            setup.oneshot(request).await
        }
    }))
}

/// `GET /System/Info/Public`, whose path Jellyfin matches ignoring case.
fn is_public_system_info(request: &Request) -> bool {
    request
        .uri()
        .path()
        .trim_end_matches('/')
        .eq_ignore_ascii_case("/System/Info/Public")
}

/// Every other request while starting: upstream's catch-all `app.Run`.
async fn starting() -> Response {
    unavailable(Some(STARTING_PAGE))
}

/// `503 Service Unavailable` with `Retry-After`, and the HTML page when given.
fn unavailable(page: Option<&'static str>) -> Response {
    let mut response = match page {
        Some(page) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/html")],
            page,
        )
            .into_response(),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER));
    response
}

/// The page a starting server answers with.
const STARTING_PAGE: &str = "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n\
<meta charset=\"utf-8\">\n<meta http-equiv=\"refresh\" content=\"5\">\n\
<title>Ferrofin is starting</title>\n</head>\n<body>\n\
<h1>Ferrofin is starting</h1>\n\
<p>The server is preparing its database. This page reloads until it is ready.</p>\n\
</body>\n</html>\n";

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::routing::get;

    use super::*;

    /// One request through the dispatcher: status, `Retry-After`, body.
    async fn send(app: &Router, path: &str) -> (StatusCode, Option<String>, String) {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).expect("request"))
            .await
            .expect("infallible");
        let retry = response
            .headers()
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().expect("ascii").to_owned());
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, retry, String::from_utf8_lossy(&body).into_owned())
    }

    /// The router a wired lifetime serves, in these tests.
    fn wired() -> Router {
        Router::new()
            .route("/Items", get(|| async { "items" }))
            .route("/System/Info/Public", get(|| async { "info" }))
    }

    #[tokio::test]
    async fn a_starting_server_answers_the_setup_responses() {
        let app = dispatcher(Ready::default());
        let (status, retry, body) = send(&app, "/Items").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(retry.as_deref(), Some("005"));
        assert!(body.contains("Ferrofin is starting"), "{body}");
        assert_eq!(send(&app, "/health").await.0, StatusCode::OK);
        assert_eq!(send(&app, "/health/live").await.0, StatusCode::OK);
        assert_eq!(
            send(&app, "/health/ready").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test(start_paused = true)]
    async fn public_system_info_waits_for_the_router_then_gives_up() {
        let app = dispatcher(Ready::default());
        let (status, retry, _) = send(&app, "/system/info/public").await;
        assert_eq!(
            (status, retry.as_deref()),
            (StatusCode::SERVICE_UNAVAILABLE, Some("005"))
        );

        let ready = Ready::default();
        let app = dispatcher(Arc::clone(&ready));
        let pending = tokio::spawn({
            let app = app.clone();
            async move { send(&app, "/System/Info/Public").await }
        });
        tokio::time::sleep(PUBLIC_INFO_RETRY_DELAY * 2).await;
        ready.set(wired()).expect("set once");
        let (status, _, body) = pending.await.expect("task");
        assert_eq!((status, body.as_str()), (StatusCode::OK, "info"));
    }

    #[tokio::test]
    async fn a_ready_server_serves_its_router() {
        let ready = Ready::default();
        ready.set(wired()).expect("set once");
        let app = dispatcher(ready);
        let (status, retry, body) = send(&app, "/Items").await;
        assert_eq!(
            (status, retry, body.as_str()),
            (StatusCode::OK, None, "items")
        );
    }
}
