//! The `X-Response-Time-ms` header, end to end through the assembled router.
//!
//! Port of `Jellyfin.Api/Middleware/ResponseTimeMiddleware.cs`: every response
//! carries the elapsed milliseconds as an invariant-culture number, whatever
//! `EnableSlowResponseWarning` says (that setting gates only the debug log of a
//! slow response).

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::response_time::{RESPONSE_TIME_HEADER, response_time_layer};
use ferrofin_api::test_support::{FakeAuthService, FakeConfig, fake_state, fake_state_with};
use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::system::ServerApplicationPaths;
use tower::ServiceExt;

/// The header's value, parsed.
async fn response_time(path: &str) -> (StatusCode, f64) {
    let response = create_router(fake_state())
        .oneshot(Request::get(path).body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let value = response
        .headers()
        .get(RESPONSE_TIME_HEADER)
        .unwrap_or_else(|| panic!("{path} carries no X-Response-Time-ms"))
        .to_str()
        .expect("ascii")
        .parse::<f64>()
        .expect("a number");
    (response.status(), value)
}

/// A handled route, an unknown path and a refused request all carry it: the
/// layer sits outside routing and authentication.
#[rstest::rstest]
#[case::handled("/System/Ping", StatusCode::OK)]
#[case::unknown("/No/Such/Route", StatusCode::NOT_FOUND)]
#[case::unauthenticated("/Users/Me", StatusCode::UNAUTHORIZED)]
#[tokio::test]
async fn every_response_carries_its_response_time(#[case] path: &str, #[case] status: StatusCode) {
    let (actual, millis) = response_time(path).await;
    assert_eq!(actual, status);
    assert!(millis >= 0.0, "{millis}");
}

/// Server configuration with the given slow-response settings; everything else
/// is [`FakeConfig`]'s.
struct SlowResponseConfig {
    enabled: bool,
    threshold_ms: i64,
}

#[async_trait::async_trait]
impl ServerConfigurationManager for SlowResponseConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        FakeConfig.application_paths()
    }
    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(Arc::new(ServerConfiguration {
            enable_slow_response_warning: self.enabled,
            slow_response_threshold_ms: self.threshold_ms,
            ..ServerConfiguration::default()
        }))
    }
    async fn update_configuration(&self, _: &ServerConfiguration) -> Result<(), ServiceError> {
        unimplemented!("not used by these tests")
    }
    async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
        unimplemented!("not used by these tests")
    }
    async fn update_branding(&self, _: &BrandingOptions) -> Result<(), ServiceError> {
        unimplemented!("not used by these tests")
    }
}

/// Everything logged while the test runs.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A slow response is logged at debug only while the warning is on and the
/// response took longer than the threshold; the header is sent regardless.
#[rstest::rstest]
#[case::on_and_over(true, -1, true)]
#[case::off(false, -1, false)]
#[case::on_and_under(true, 3_600_000, false)]
#[tokio::test]
async fn a_slow_response_is_logged_only_when_the_warning_is_on_and_exceeded(
    #[case] enabled: bool,
    #[case] threshold_ms: i64,
    #[case] logged: bool,
) {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let state = fake_state_with(
        Arc::new(FakeAuthService),
        Arc::new(SlowResponseConfig {
            enabled,
            threshold_ms,
        }),
    );
    let router = axum::Router::new()
        .route("/slow", axum::routing::get(|| async { "done" }))
        .layer(axum::middleware::from_fn_with_state(
            state,
            response_time_layer,
        ));
    let response = router
        .oneshot(
            Request::get("/slow?api_key=secret")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert!(response.headers().contains_key(RESPONSE_TIME_HEADER));
    let log = String::from_utf8(captured.0.lock().expect("log").clone()).expect("utf-8");
    assert_eq!(log.contains("slow HTTP response"), logged, "{log}");
    if logged {
        assert!(
            log.contains("path=/slow") && log.contains("status=200"),
            "{log}"
        );
    }
    assert!(!log.contains("secret"), "{log}");
}
