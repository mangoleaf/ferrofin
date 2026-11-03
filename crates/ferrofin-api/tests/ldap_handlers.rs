//! Handler tests for the LDAP-Auth plugin's settings-page routes
//! (`handlers::ldap`, upstream `LdapController.TestServerBind`,
//! `TestLdapFilters` and `LdapUserSearch`).
//!
//! Each test drives a `POST /Ldap/*` route through the real router against a
//! [`FakeLdap`] manager, asserting the status, the body and whether the
//! manager was reached. The caller is an API key (an administrator), a plain
//! user, or nobody.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    authed_state_with_plugins, fake_state, user_authed_state_with_plugins,
};
use ferrofin_model::ldap::{
    LdapFilterResponse, LdapTestErrorResponse, ServerConnectionInfo, ServerTestResponse,
    UserFilterInfo, UserSearchAttributes, UserSearchResponse,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::ldap::{LdapManager, LdapTestAnswer};
use ferrofin_traits::plugins::DisabledPluginManager;
use tower::ServiceExt;

/// An [`LdapManager`] fake: enabled or not, answering fixed test results and
/// keeping the bodies it was given.
struct FakeLdap {
    enabled: bool,
    tested: Mutex<Vec<ServerConnectionInfo>>,
    filters: Mutex<Vec<UserFilterInfo>>,
    searches: Mutex<Vec<UserSearchAttributes>>,
    filter_answer: LdapTestAnswer<LdapFilterResponse>,
    search_answer: LdapTestAnswer<UserSearchResponse>,
}

impl FakeLdap {
    fn new(enabled: bool) -> Arc<Self> {
        Self::answering(
            enabled,
            LdapTestAnswer::Ok(LdapFilterResponse {
                users: 2,
                admins: 1,
                is_subset: true,
            }),
            LdapTestAnswer::Ok(UserSearchResponse {
                located_dn: Some("uid=ada,dc=example,dc=org".to_owned()),
            }),
        )
    }

    fn answering(
        enabled: bool,
        filter_answer: LdapTestAnswer<LdapFilterResponse>,
        search_answer: LdapTestAnswer<UserSearchResponse>,
    ) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            tested: Mutex::new(Vec::new()),
            filters: Mutex::new(Vec::new()),
            searches: Mutex::new(Vec::new()),
            filter_answer,
            search_answer,
        })
    }

    fn tested(&self) -> Vec<ServerConnectionInfo> {
        self.tested.lock().expect("tested").clone()
    }

    fn filters(&self) -> Vec<UserFilterInfo> {
        self.filters.lock().expect("filters").clone()
    }

    fn searches(&self) -> Vec<UserSearchAttributes> {
        self.searches.lock().expect("searches").clone()
    }

    /// Whether any route reached the manager past its enabled check.
    fn untouched(&self) -> bool {
        self.tested().is_empty() && self.filters().is_empty() && self.searches().is_empty()
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

    async fn test_ldap_filters(
        &self,
        filters: UserFilterInfo,
    ) -> Result<LdapTestAnswer<LdapFilterResponse>, ServiceError> {
        self.ensure_enabled().await?;
        self.filters.lock().expect("filters").push(filters);
        Ok(self.filter_answer.clone())
    }

    async fn ldap_user_search(
        &self,
        search: UserSearchAttributes,
    ) -> Result<LdapTestAnswer<UserSearchResponse>, ServiceError> {
        self.ensure_enabled().await?;
        self.searches.lock().expect("searches").push(search);
        Ok(self.search_answer.clone())
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

/// Each route with the body its settings-page button sends.
const PAGE_REQUESTS: [(&str, &str); 3] = [
    ("/Ldap/TestServerBind", PAGE_BODY),
    ("/Ldap/TestLdapFilters", FILTERS_BODY),
    ("/Ldap/LdapUserSearch", SEARCH_BODY),
];

#[tokio::test]
async fn a_plain_user_is_refused_before_the_plugin_is_reached() {
    for (path, body) in PAGE_REQUESTS {
        let fake = FakeLdap::new(true);
        let state =
            user_authed_state_with_plugins(Arc::new(DisabledPluginManager)).with_ldap(fake.clone());
        let (status, _) = send(state, post(path, body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        assert!(fake.untouched(), "{path}");
    }
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
    for (path, page_body) in PAGE_REQUESTS {
        for body in [page_body, "{}", "not json"] {
            let fake = FakeLdap::new(false);
            let (status, _) = send(as_admin(fake.clone()), post(path, body)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path} {body}");
            assert!(fake.untouched(), "{path}");
        }
    }
}

#[tokio::test]
async fn a_server_without_the_extension_answers_not_found() {
    for (path, body) in PAGE_REQUESTS {
        let state = authed_state_with_plugins(Arc::new(DisabledPluginManager));
        let (status, _) = send(state, post(path, body)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
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
    for (path, _) in PAGE_REQUESTS {
        assert!(
            PLUGIN_ROUTES.contains(&("post", path, "ldap-auth")),
            "{path}"
        );
    }
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

/// What the settings page's filter test sends: the two filters, never the
/// member-uid flag, and `_disabled_` for an empty admin filter box.
const FILTERS_BODY: &str =
    r#"{"LdapSearchFilter":"(objectClass=inetOrgPerson)","LdapAdminFilter":"_disabled_"}"#;

/// What the settings page's user search sends.
const SEARCH_BODY: &str = r#"{"LdapSearchAttributes":"uid, mail","TestSearchUsername":"ada"}"#;

#[tokio::test]
async fn the_filter_test_answers_the_counts_and_the_filters_reach_the_plugin() {
    let fake = FakeLdap::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        post("/Ldap/TestLdapFilters", FILTERS_BODY),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({"Users": 2, "Admins": 1, "IsSubset": true})
    );
    assert_eq!(
        fake.filters(),
        vec![UserFilterInfo {
            ldap_search_filter: Some("(objectClass=inetOrgPerson)".to_owned()),
            ldap_admin_filter: Some("_disabled_".to_owned()),
            enable_ldap_admin_filter_member_uid: None,
        }]
    );
}

#[tokio::test]
async fn a_failed_filter_test_carries_upstreams_message() {
    for (answer, status, message) in [
        (
            LdapTestAnswer::BadRequest(LdapTestErrorResponse::new("User Filter: Filter Error")),
            StatusCode::BAD_REQUEST,
            "User Filter: Filter Error",
        ),
        (
            LdapTestAnswer::Unauthorized(LdapTestErrorResponse::new(
                "Failed to Connect or Bind to server.",
            )),
            StatusCode::UNAUTHORIZED,
            "Failed to Connect or Bind to server.",
        ),
    ] {
        let fake = FakeLdap::answering(
            true,
            answer,
            LdapTestAnswer::Ok(UserSearchResponse::default()),
        );
        let (got, body) = send(as_admin(fake), post("/Ldap/TestLdapFilters", FILTERS_BODY)).await;
        assert_eq!(got, status);
        assert_eq!(body, serde_json::json!({"Message": message}));
    }
}

#[tokio::test]
async fn a_filter_body_that_does_not_bind_is_a_validation_problem() {
    let fake = FakeLdap::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        post(
            "/Ldap/TestLdapFilters",
            r#"{"LdapSearchFilter":"(uid=*)","EnableLdapAdminFilterMemberUid":null}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["errors"]
            .get("$.EnableLdapAdminFilterMemberUid")
            .is_some(),
        "{body}"
    );
    assert!(fake.untouched());
}

#[tokio::test]
async fn the_user_search_answers_the_located_dn() {
    let fake = FakeLdap::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        post("/Ldap/LdapUserSearch", SEARCH_BODY),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({"LocatedDn": "uid=ada,dc=example,dc=org"})
    );
    assert_eq!(
        fake.searches(),
        vec![UserSearchAttributes {
            ldap_search_attributes: Some("uid, mail".to_owned()),
            test_search_username: Some("ada".to_owned()),
        }]
    );

    // Without a name: an empty object, which the page reads as no user found.
    let empty = FakeLdap::answering(
        true,
        LdapTestAnswer::Ok(LdapFilterResponse::default()),
        LdapTestAnswer::Ok(UserSearchResponse::default()),
    );
    let (status, body) = send(
        as_admin(empty),
        post("/Ldap/LdapUserSearch", r#"{"LdapSearchAttributes":"uid"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({}));
}

#[tokio::test]
async fn a_user_search_that_finds_no_one_is_unauthorized_with_upstreams_message() {
    let fake = FakeLdap::answering(
        true,
        LdapTestAnswer::Ok(LdapFilterResponse::default()),
        LdapTestAnswer::Unauthorized(LdapTestErrorResponse::new(
            "Found no LDAP users matching provided username.",
        )),
    );
    let (status, body) = send(as_admin(fake), post("/Ldap/LdapUserSearch", SEARCH_BODY)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body,
        serde_json::json!({"Message": "Found no LDAP users matching provided username."})
    );
}
