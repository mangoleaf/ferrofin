//! What the authentication path hands the authorization logic.
//!
//! The context the auth layer builds for every request carries the headers the
//! parser reads and no others: copying a browser's whole header set for a parser
//! that reads four was a constant tax on every image and segment fetch.

use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::router::create_router;
use ferrofin_api::test_support::{FakeConfig, fake_state_with};
use ferrofin_networking::{NetworkConfiguration, NetworkManager};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::{AuthService, RequestContext};
use ferrofin_traits::options::AuthorizationInfo;
use tower::ServiceExt as _;

/// Records the context it is handed, then refuses it.
#[derive(Default)]
struct RecordingAuth {
    seen: Mutex<Option<RequestContext>>,
}

#[async_trait]
impl AuthService for RecordingAuth {
    async fn authenticate(
        &self,
        request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        *self.seen.lock().expect("lock") = Some(request.clone());
        Err(ServiceError::unauthorized("recorded"))
    }
}

/// `GET /Users/Me?{query}` with `headers`, returning the context the auth layer
/// built.
async fn context_for(headers: &[(&str, &str)], query: &str) -> RequestContext {
    let auth = Arc::new(RecordingAuth::default());
    let state = fake_state_with(
        Arc::clone(&auth) as Arc<dyn AuthService>,
        Arc::new(FakeConfig),
    );
    let mut request = Request::builder().uri(format!("/Users/Me?{query}"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = create_router(state)
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    auth.seen
        .lock()
        .expect("lock")
        .take()
        .expect("authenticated")
}

/// Everything but the four credential headers stays out of the context.
#[tokio::test]
async fn the_auth_context_carries_only_the_credential_headers() {
    let context = context_for(
        &[
            ("host", "media.example.org"),
            ("user-agent", "Mozilla/5.0"),
            ("accept", "application/json"),
            ("accept-language", "sv"),
            ("cookie", "session=abc"),
            ("x-forwarded-for", "203.0.113.9"),
            ("authorization", "MediaBrowser Token=\"a\""),
            ("x-emby-authorization", "MediaBrowser Token=\"b\""),
            ("x-emby-token", "c"),
            ("x-mediabrowser-token", "d"),
        ],
        "ApiKey=e",
    )
    .await;
    let mut names: Vec<&str> = context.headers.iter().map(|(n, _)| n.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "authorization",
            "x-emby-authorization",
            "x-emby-token",
            "x-mediabrowser-token",
        ]
    );
    assert_eq!(context.header("X-Emby-Token"), Some("c"));
    assert_eq!(context.query_string.as_deref(), Some("ApiKey=e"));
    assert!(context.remote_endpoint.is_some(), "the caller's address");
}

/// Repeated values reach the parser in the order the client sent them, since
/// it reads the first.
#[tokio::test]
async fn repeated_credential_headers_keep_their_order() {
    let context = context_for(
        &[
            ("authorization", "first"),
            ("user-agent", "Mozilla/5.0"),
            ("authorization", "second"),
        ],
        "",
    )
    .await;
    assert_eq!(context.header("authorization"), Some("first"));
    let values: Vec<&str> = context.headers.iter().map(|(_, v)| v.as_str()).collect();
    assert_eq!(values, ["first", "second"]);
}

/// Behind a known proxy the context names the forwarded client, the address the
/// IP gate in front of the auth layer filtered.
#[tokio::test]
async fn the_auth_context_names_the_client_the_ip_gate_resolved() {
    let auth = Arc::new(RecordingAuth::default());
    let network = NetworkManager::with_defaults(
        NetworkConfiguration {
            known_proxies: vec!["10.0.0.0/8".to_owned()],
            ..NetworkConfiguration::default()
        },
        "",
    );
    let state = fake_state_with(
        Arc::clone(&auth) as Arc<dyn AuthService>,
        Arc::new(FakeConfig),
    )
    .with_network(Arc::new(RwLock::new(network)));
    let mut request = Request::builder()
        .uri("/Users/Me")
        .header("x-forwarded-for", "203.0.113.7")
        .body(Body::empty())
        .expect("request");
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "10.4.0.9:5000"
            .parse::<std::net::SocketAddr>()
            .expect("peer"),
    ));
    let response = create_router(state)
        .oneshot(request)
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let context = auth
        .seen
        .lock()
        .expect("lock")
        .take()
        .expect("authenticated");
    assert_eq!(context.remote_endpoint.as_deref(), Some("203.0.113.7"));
}
