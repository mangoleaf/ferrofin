//! Handler tests for the LDAP-Auth plugin's settings-page route
//! (`handlers::ldap`, upstream `LdapController.TestServerBind`).
//!
//! Each test drives `POST /Ldap/TestServerBind` through the real router
//! against a [`FakeLdap`] manager, asserting the status, the body and whether
//! the manager was reached. The caller is an API key (an administrator), a
//! plain user, or nobody.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    authed_state_with_plugins, fake_state, user_authed_state_with_plugins,
};
use ferrofin_model::ldap::{ServerConnectionInfo, ServerTestResponse};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::ldap::LdapManager;
use ferrofin_traits::plugins::DisabledPluginManager;
use tower::ServiceExt;

/// An [`LdapManager`] fake: enabled or not, answering a fixed test result and
/// keeping the connection settings it was given.
struct FakeLdap {
    enabled: bool,
    tested: Mutex<Vec<ServerConnectionInfo>>,
}

impl FakeLdap {
    fn new(enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            tested: Mutex::new(Vec::new()),
        })
    }

    fn tested(&self) -> Vec<ServerConnectionInfo> {
        self.tested.lock().expect("tested").clone()
    }
}

/// What [`FakeLdap`] answers: the bind failed.
fn wrong_password() -> ServerTestResponse {
    ServerTestResponse {
        connect: Some("Success".to_owned()),
        bind: Some("Testing...".to_owned()),
        error: Some("Invalid Credentials".to_owned()),
        ..ServerTestResponse::default()
    }
}

#[async_trait::async_trait]
impl LdapManager for FakeLdap {
    async fn ensure_enabled(&self) -> Result<(), ServiceError> {
        if self.enabled {
            Ok(())
        } else {
            Err(ServiceError::not_found("the LDAP-Auth plugin is disabled"))
        }
    }

    async fn test_server_bind(
        &self,
        connection: ServerConnectionInfo,
    ) -> Result<ServerTestResponse, ServiceError> {
        self.ensure_enabled().await?;
        self.tested.lock().expect("tested").push(connection);
        Ok(wrong_password())
    }
}

fn as_admin(fake: Arc<FakeLdap>) -> AppState {
    authed_state_with_plugins(Arc::new(DisabledPluginManager)).with_ldap(fake)
}

/// What the settings page sends: every member, `LdapPort` as a number.
const PAGE_BODY: &str = r#"{"LdapServer":"ldap.example","LdapPort":636,"UseSsl":true,
    "AllowPassChange":false,"UseStartTls":false,"SkipSslVerify":false,
    "LdapBindUser":"cn=reader,dc=example,dc=org","LdapBindPassword":"reader-secret",
    "LdapBaseDn":"dc=example,dc=org","PasswordResetUrl":"","LdapClientCertPath":"",
    "LdapClientKeyPath":"","LdapRootCaPath":"/etc/ssl/ldap-ca.pem"}"#;

fn post(path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("Authorization", "Token abc")
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_owned()))
        .expect("request")
}

async fn send(state: AppState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = create_router(state)
        .oneshot(request)
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
        })
    };
    (status, body)
}

#[tokio::test]
async fn an_admin_gets_each_steps_result_and_the_settings_reach_the_plugin() {
    let fake = FakeLdap::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        post("/Ldap/TestServerBind", PAGE_BODY),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({"Connect": "Success", "Bind": "Testing...", "Error": "Invalid Credentials"}),
        "PascalCase, and the steps never reached are absent"
    );
    let tested = fake.tested();
    assert_eq!(tested.len(), 1);
    assert_eq!(tested[0].ldap_server.as_deref(), Some("ldap.example"));
    assert_eq!(tested[0].ldap_port, 636);
    assert!(tested[0].use_ssl);
    assert_eq!(
        tested[0]
            .ldap_bind_password
            .as_ref()
            .map(|p| p.expose().to_owned()),
        Some("reader-secret".to_owned())
    );
    assert_eq!(
        tested[0].ldap_root_ca_path.as_deref(),
        Some("/etc/ssl/ldap-ca.pem")
    );
}

#[tokio::test]
async fn a_port_sent_as_text_binds_as_a_number() {
    let fake = FakeLdap::new(true);
    let (status, _) = send(
        as_admin(fake.clone()),
        post(
            "/Ldap/TestServerBind",
            r#"{"LdapServer":"ldap.example","LdapPort":"389","LdapBaseDn":"dc=example"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fake.tested()[0].ldap_port, 389);
}

#[tokio::test]
async fn a_body_without_the_required_members_is_a_validation_problem() {
    let fake = FakeLdap::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        post(
            "/Ldap/TestServerBind",
            r#"{"LdapServer":"  ","LdapPort":389}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["title"], "One or more validation errors occurred.");
    assert_eq!(
        body["errors"],
        serde_json::json!({
            "LdapServer": ["The LdapServer field is required."],
            "LdapBaseDn": ["The LdapBaseDn field is required."]
        })
    );
    assert_eq!(fake.tested(), Vec::new(), "nothing is saved or tested");
}

#[tokio::test]
async fn a_body_that_does_not_bind_is_a_validation_problem() {
    let fake = FakeLdap::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        post(
            "/Ldap/TestServerBind",
            r#"{"LdapServer":"ldap.example","UseSsl":null}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["errors"].get("$.UseSsl").is_some(), "{body}");
    assert_eq!(fake.tested(), Vec::new());
}

#[tokio::test]
async fn a_plain_user_is_refused_before_the_plugin_is_reached() {
    let fake = FakeLdap::new(true);
    let state =
        user_authed_state_with_plugins(Arc::new(DisabledPluginManager)).with_ldap(fake.clone());
    let (status, _) = send(state, post("/Ldap/TestServerBind", PAGE_BODY)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(fake.tested(), Vec::new());
}

#[tokio::test]
async fn an_anonymous_caller_is_unauthorized() {
    let fake = FakeLdap::new(true);
    let request = Request::builder()
        .method("POST")
        .uri("/Ldap/TestServerBind")
        .header("Content-Type", "application/json")
        .body(Body::from(PAGE_BODY))
        .expect("request");
    let (status, _) = send(fake_state().with_ldap(fake.clone()), request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(fake.tested(), Vec::new());
}

#[tokio::test]
async fn a_disabled_plugin_answers_not_found_whatever_the_body() {
    for body in [PAGE_BODY, "{}", "not json"] {
        let fake = FakeLdap::new(false);
        let (status, _) = send(as_admin(fake.clone()), post("/Ldap/TestServerBind", body)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(fake.tested(), Vec::new());
    }
}

#[tokio::test]
async fn a_server_without_the_extension_answers_not_found() {
    let state = authed_state_with_plugins(Arc::new(DisabledPluginManager));
    let (status, _) = send(state, post("/Ldap/TestServerBind", PAGE_BODY)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn every_plugin_route_is_registered() {
    use ferrofin_api::handlers::PLUGIN_ROUTES;
    // Every row is LDAP-Auth's so far; another plugin's rows need its manager
    // wired here too.
    for (method, path, _) in PLUGIN_ROUTES {
        let request = Request::builder()
            .method(method.to_ascii_uppercase().as_str())
            .uri(*path)
            .header("Authorization", "Token abc")
            .header("Content-Type", "application/json")
            .body(Body::from("{}"))
            .expect("request");
        let (status, _) = send(as_admin(FakeLdap::new(true)), request).await;
        assert_ne!(status, StatusCode::NOT_FOUND, "{method} {path}");
        assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
    }
}

#[test]
fn the_route_is_a_plugin_route_outside_the_contract() {
    use ferrofin_api::handlers::{EXTENSION_ROUTES, PLUGIN_ROUTES, REAL_ROUTES};
    assert!(PLUGIN_ROUTES.contains(&("post", "/Ldap/TestServerBind", "ldap-auth")));
    for (method, path, _) in PLUGIN_ROUTES {
        assert!(!REAL_ROUTES.contains(&(*method, *path)), "{path}");
        assert!(
            !EXTENSION_ROUTES
                .iter()
                .any(|(m, p, _)| m == method && p == path),
            "{path}"
        );
    }
}
