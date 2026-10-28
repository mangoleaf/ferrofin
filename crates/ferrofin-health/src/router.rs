//! The `health_router` mounting the liveness and readiness endpoints.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum::{Json, Router};

use crate::checker::HealthChecker;
use crate::response::{LiveResponse, ReadyResponse};

/// Shared, cheaply-cloneable list of checkers held as router state, each with
/// whether it failed on its last run.
type Checkers = Arc<Vec<(Arc<dyn HealthChecker>, AtomicBool)>>;

/// Builds a router exposing the probe endpoints:
///
/// - `/health` (and `/health/`), Jellyfin's `MapHealthChecks("/health")` with
///   ASP.NET Core's default options: runs every checker in `checkers`, then
///   answers plain-text `Healthy` (`200 OK`) or `Unhealthy`
///   (`503 Service Unavailable`), never cached, to any method.
/// - `GET /health/live` — process-only liveness, always `200 OK`. It never runs
///   `checkers`, so a dependency outage cannot restart the process.
/// - `GET /health/ready` — runs every checker in `checkers`; returns `200 OK`
///   when all pass, or `503 Service Unavailable` with the failing names when any
///   fail.
///
/// The returned [`Router`] carries no other state and can be `.merge`d into a
/// service's main router.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
///
/// use ferrofin_health::{FnChecker, HealthChecker, health_router};
///
/// let checkers: Vec<Arc<dyn HealthChecker>> =
///     vec![Arc::new(FnChecker::new("database", || async { Ok(()) }))];
/// let _router = health_router(checkers);
/// ```
pub fn health_router(checkers: Vec<Arc<dyn HealthChecker>>) -> Router {
    let checkers: Checkers = Arc::new(
        checkers
            .into_iter()
            .map(|checker| (checker, AtomicBool::new(false)))
            .collect(),
    );

    Router::new()
        .route("/health", any(health))
        .route("/health/", any(health))
        .route("/health/live", get(liveness))
        .route("/health/ready", get(readiness))
        .with_state(checkers)
}

/// The name [`startup_router`]'s readiness reports as failing.
pub const STARTUP_CHECK: &str = "startup";

/// The probe endpoints while the server is still starting (port of the
/// setup server's `UseHealthChecks("/health")` over `SetupHealthcheck`):
///
/// - `/health` (and `/health/`) answers plain-text `Degraded` with `200 OK`,
///   as ASP.NET Core maps a `Degraded` result ("Server is still starting
///   up.");
/// - `GET /health/live` is the same process-only `200 OK` as
///   [`health_router`]'s, so a long boot is never restarted;
/// - `GET /health/ready` is `503 Service Unavailable`, failing
///   [`STARTUP_CHECK`], so no traffic is routed before the server is ready.
pub fn startup_router() -> Router {
    Router::new()
        .route("/health", any(starting))
        .route("/health/", any(starting))
        .route("/health/live", get(liveness))
        .route("/health/ready", get(not_ready_while_starting))
}

/// `/health` while starting: `Degraded`, which ASP.NET Core answers with 200.
async fn starting() -> impl IntoResponse {
    plaintext(StatusCode::OK, "Degraded")
}

/// `GET /health/ready` while starting.
async fn not_ready_while_starting() -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ReadyResponse {
            status: "not_ready",
            failing: vec![STARTUP_CHECK.to_owned()],
        }),
    )
}

/// `HealthCheckMiddleware`'s `WriteMinimalPlaintext` answer: the status name
/// as `text/plain`, with the headers it sets when `AllowCachingResponses` is
/// off.
fn plaintext(status: StatusCode, text: &'static str) -> impl IntoResponse {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/plain"),
            (header::CACHE_CONTROL, "no-store, no-cache"),
            (header::PRAGMA, "no-cache"),
            (header::EXPIRES, "Thu, 01 Jan 1970 00:00:00 GMT"),
        ],
        text,
    )
}

/// Handles `GET /health/live`: reports the process is up without probing
/// dependencies, plus the compile-time build identity of the running binary.
async fn liveness() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(LiveResponse {
            status: "ok",
            build: crate::build_version(),
        }),
    )
}

/// Runs every checker in order and returns the names of those that failed.
async fn failing_checks(checkers: &Checkers) -> Vec<String> {
    let mut failing = Vec::new();
    for (checker, was_failing) in checkers.iter() {
        match checker.check().await {
            Ok(()) => {
                if was_failing.swap(false, Ordering::Relaxed) {
                    tracing::info!(check = checker.name(), "health check recovered");
                }
            }
            Err(reason) => {
                // The reason is dropped from the payload (only names are
                // reported), so it is logged; at warn only when the check
                // starts failing, since probes repeat it at their own rate.
                if was_failing.swap(true, Ordering::Relaxed) {
                    tracing::debug!(check = checker.name(), %reason, "health check still failing");
                } else {
                    tracing::warn!(check = checker.name(), %reason, "health check failed");
                }
                failing.push(checker.name().to_string());
            }
        }
    }
    failing
}

/// Handles `/health` as ASP.NET Core's `HealthCheckMiddleware` does with
/// default options: `WriteMinimalPlaintext` (the status name as
/// `text/plain`), `Unhealthy` mapped to 503, and the headers it sets when
/// `AllowCachingResponses` is off.
async fn health(State(checkers): State<Checkers>) -> impl IntoResponse {
    if failing_checks(&checkers).await.is_empty() {
        plaintext(StatusCode::OK, "Healthy")
    } else {
        plaintext(StatusCode::SERVICE_UNAVAILABLE, "Unhealthy")
    }
}

/// Handles `GET /health/ready`: runs every checker and aggregates the outcome.
async fn readiness(State(checkers): State<Checkers>) -> impl IntoResponse {
    let failing = failing_checks(&checkers).await;
    if failing.is_empty() {
        (
            StatusCode::OK,
            Json(ReadyResponse {
                status: "ready",
                failing,
            }),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReadyResponse {
                status: "not_ready",
                failing,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::checker::FnChecker;

    /// Drives one request through the router and returns the status + JSON body.
    async fn request(router: Router, path: &str) -> (StatusCode, serde_json::Value) {
        let response = router
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap();
        (status, json)
    }

    /// While starting: `/health` is `Degraded` (200), liveness holds, and
    /// readiness fails the startup check.
    #[tokio::test]
    async fn the_startup_router_is_live_degraded_and_not_ready() {
        let response = startup_router()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let text = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&text[..], b"Degraded");
        let (status, body) = request(startup_router(), "/health/live").await;
        assert_eq!(
            (status, body["status"].clone()),
            (StatusCode::OK, "ok".into())
        );
        let (status, body) = request(startup_router(), "/health/ready").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["failing"], serde_json::json!([STARTUP_CHECK]));
    }

    #[tokio::test]
    async fn liveness_always_ok() {
        let (status, body) = request(health_router(vec![]), "/health/live").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        // The baked build identity is always present and non-empty — the
        // benchmark harness aborts a run when it can't verify the binary.
        assert_ne!(body["build"].as_str().unwrap(), "");
        assert_eq!(body["build"], crate::build_version());
    }

    #[tokio::test]
    async fn readiness_no_checkers_is_ready() {
        let (status, body) = request(health_router(vec![]), "/health/ready").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ready");
        assert_eq!(body["failing"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn readiness_all_ok_returns_200() {
        let checkers: Vec<Arc<dyn HealthChecker>> = vec![
            Arc::new(FnChecker::new("database", || async { Ok(()) })),
            Arc::new(FnChecker::new("storage", || async { Ok(()) })),
        ];
        let (status, body) = request(health_router(checkers), "/health/ready").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ready");
        assert_eq!(body["failing"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn readiness_one_failing_returns_503_with_name() {
        let checkers: Vec<Arc<dyn HealthChecker>> = vec![
            Arc::new(FnChecker::new("database", || async { Ok(()) })),
            Arc::new(FnChecker::new("storage", || async {
                Err("connection refused".to_string())
            })),
        ];
        let (status, body) = request(health_router(checkers), "/health/ready").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["failing"], serde_json::json!(["storage"]));
    }

    /// Drives one request through the router and returns the status, headers
    /// and body text.
    async fn request_text(
        router: Router,
        path: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let response = router
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        (
            parts.status,
            parts.headers,
            String::from_utf8(bytes.to_vec()).unwrap(),
        )
    }

    /// Jellyfin's `MapHealthChecks("/health")` with ASP.NET Core's defaults:
    /// the minimal plain-text writer, `Unhealthy` as 503, and no caching, with
    /// or without a trailing slash.
    #[tokio::test]
    async fn health_reports_plain_text_like_jellyfin() {
        let ok: Vec<Arc<dyn HealthChecker>> =
            vec![Arc::new(FnChecker::new("database", || async { Ok(()) }))];
        for path in ["/health", "/health/"] {
            let (status, headers, body) = request_text(health_router(ok.clone()), path).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(body, "Healthy");
            // Upstream maps every method, not just GET.
            for method in ["HEAD", "POST"] {
                let response = health_router(ok.clone())
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(path)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{method} {path}");
            }
            assert_eq!(headers["content-type"], "text/plain");
            assert_eq!(headers["cache-control"], "no-store, no-cache");
            assert_eq!(headers["pragma"], "no-cache");
            assert_eq!(headers["expires"], "Thu, 01 Jan 1970 00:00:00 GMT");
        }

        let failing: Vec<Arc<dyn HealthChecker>> =
            vec![Arc::new(FnChecker::new("database", || async {
                Err("unreachable".to_string())
            }))];
        let (status, _, body) = request_text(health_router(failing), "/health").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "Unhealthy");
    }
}
