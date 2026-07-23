//! First-Light handler behaviour tests, using fake `hermit-traits` impls.
//!
//! These exercise the *real* handlers end to end through the assembled router:
//! the public system-info route projects a manager value to JSON, and the
//! authenticated routes reject a tokenless request with `401`. Domain managers
//! not under test reuse the `test_support` fakes (which panic if called), so a
//! handler straying onto an unexpected manager is caught.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeAuthContext, FakeAuthService, FakeConfig, FakeDto, FakeLibrary, FakeMediaSources,
    FakeSessions, FakeUserData, FakeUserViews, FakeUsers,
};
use hermit_model::system::{PublicSystemInfo, SystemInfo};
use hermit_traits::error::ServiceError;
use hermit_traits::net::RequestContext;
use hermit_traits::system::{ServerApplicationHost, SystemManager};
use tower::ServiceExt;

/// A [`SystemManager`] that returns canned info, letting the real handler run.
struct StubSystem {
    server_name: String,
}

#[async_trait]
impl SystemManager for StubSystem {
    async fn get_system_info(&self, _request: &RequestContext) -> Result<SystemInfo, ServiceError> {
        Ok(SystemInfo {
            server_name: Some(self.server_name.clone()),
            ..SystemInfo::default()
        })
    }
    async fn get_public_system_info(
        &self,
        _request: &RequestContext,
    ) -> Result<PublicSystemInfo, ServiceError> {
        Ok(PublicSystemInfo {
            server_name: Some(self.server_name.clone()),
            ..PublicSystemInfo::default()
        })
    }
    async fn restart(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn shutdown(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_system_storage_info(
        &self,
    ) -> Result<hermit_model::system::SystemStorageInfo, ServiceError> {
        unimplemented!()
    }
}

/// A minimal [`ServerApplicationHost`] with neutral getters.
struct StubHost;

#[async_trait]
impl ServerApplicationHost for StubHost {
    fn core_startup_has_completed(&self) -> bool {
        true
    }
    fn http_port(&self) -> u16 {
        8096
    }
    fn https_port(&self) -> u16 {
        8920
    }
    fn listen_with_https(&self) -> bool {
        false
    }
    fn friendly_name(&self) -> String {
        "hermit-test".to_owned()
    }
    async fn get_smart_api_url(&self, _request: &RequestContext) -> Result<String, ServiceError> {
        unimplemented!()
    }
    async fn get_local_api_url(
        &self,
        _hostname: &str,
        _scheme: Option<&str>,
        _port: Option<u16>,
    ) -> Result<String, ServiceError> {
        unimplemented!()
    }
    fn expand_virtual_path(&self, path: &str) -> String {
        path.to_owned()
    }
    fn reverse_virtual_path(&self, path: &str) -> String {
        path.to_owned()
    }
}

/// Builds an [`AppState`] whose system manager is the canned [`StubSystem`];
/// every other manager is a `test_support` fake.
fn state_with_system(server_name: &str) -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(StubSystem {
            server_name: server_name.to_owned(),
        }),
        Arc::new(StubHost),
        Arc::new(FakeConfig),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(FakeAuthService),
    )
}

#[tokio::test]
async fn public_system_info_is_served_without_auth() {
    let router = create_router(state_with_system("Hermit"));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/System/Info/Public")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["ServerName"], "Hermit");
}

#[tokio::test]
async fn system_info_requires_auth() {
    // The fake auth service rejects the tokenless request, so the authenticated
    // full-info route is `401` — proving the route exists and is guarded.
    let router = create_router(state_with_system("Hermit"));
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
async fn me_requires_auth() {
    let router = create_router(state_with_system("Hermit"));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/Users/Me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
