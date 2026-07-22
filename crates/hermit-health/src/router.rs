//! The `health_router` mounting the liveness and readiness endpoints.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::checker::HealthChecker;
use crate::response::{LiveResponse, ReadyResponse};

/// Shared, cheaply-cloneable list of readiness checkers held as router state.
type Checkers = Arc<Vec<Arc<dyn HealthChecker>>>;

/// Builds a router exposing the two standard probe endpoints:
///
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
/// use hermit_health::{FnChecker, HealthChecker, health_router};
///
/// let checkers: Vec<Arc<dyn HealthChecker>> =
///     vec![Arc::new(FnChecker::new("database", || async { Ok(()) }))];
/// let _router = health_router(checkers);
/// ```
pub fn health_router(checkers: Vec<Arc<dyn HealthChecker>>) -> Router {
    let checkers: Checkers = Arc::new(checkers);

    Router::new()
        .route("/health/live", get(liveness))
        .route("/health/ready", get(readiness))
        .with_state(checkers)
}

/// Handles `GET /health/live`: reports the process is up without probing
/// dependencies.
async fn liveness() -> impl IntoResponse {
    (StatusCode::OK, Json(LiveResponse { status: "ok" }))
}

/// Handles `GET /health/ready`: runs every checker and aggregates the outcome.
async fn readiness(State(checkers): State<Checkers>) -> impl IntoResponse {
    let mut failing = Vec::new();
    for checker in checkers.iter() {
        if let Err(reason) = checker.check().await {
            // The reason is dropped from the payload (only names are reported),
            // but tracing it keeps the failure diagnosable server-side.
            tracing::warn!(check = checker.name(), %reason, "readiness check failed");
            failing.push(checker.name().to_string());
        }
    }

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

    #[tokio::test]
    async fn liveness_always_ok() {
        let (status, body) = request(health_router(vec![]), "/health/live").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
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
}
