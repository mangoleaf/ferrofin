//! Handler tests for the Single Sign-On plugin's routes (`handlers::sso`):
//! the provider settings, `GET /sso/admin/oidc/providers` and
//! `PUT`/`DELETE /sso/admin/oidc/providers/{provider}`, and the sign-in,
//! `GET /sso/oidc/{provider}/start` and `GET /sso/oidc/{provider}/callback`.
//!
//! Each test drives a route through the real router against a [`FakeSso`]
//! manager, asserting the status, the body and whether the manager was
//! reached. The caller is an API key (an administrator), a plain user, or
//! nobody.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    authed_state_with_plugins, fake_state, user_authed_state_with_plugins,
};
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{OidcDiscovery, OidcProvider, OidcProviderInfo, OidcProviderList};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::DisabledPluginManager;
use ferrofin_traits::sso::{OIDC_FLOW_COOKIE, OidcCallback, SsoManager, SsoReply};
use tower::ServiceExt;

const SECRET: &str = "route-client-secret-31";

/// Where [`FakeSso`] sends a started sign-in.
const AUTHORIZE: &str = "https://id.example.org/authorize?state=s1";

/// An [`SsoManager`] fake: enabled or not, refusing a save with `refusal`
/// when set, answering a discovery test with `discovery_error` when set, and
/// keeping what it was given.
#[derive(Default)]
struct FakeSso {
    enabled: bool,
    refusal: Option<String>,
    discovery_error: Option<String>,
    saved: Mutex<Vec<(String, OidcProvider)>>,
    tested: Mutex<Vec<OidcProvider>>,
    deleted: Mutex<Vec<String>>,
    listed: Mutex<usize>,
    started: Mutex<Vec<(String, IpAddr)>>,
    finished: Mutex<Vec<(String, OidcCallback)>>,
}

impl FakeSso {
    fn new(enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            ..Self::default()
        })
    }

    fn saved(&self) -> Vec<(String, OidcProvider)> {
        self.saved.lock().expect("saved").clone()
    }

    fn deleted(&self) -> Vec<String> {
        self.deleted.lock().expect("deleted").clone()
    }

    fn tested(&self) -> Vec<OidcProvider> {
        self.tested.lock().expect("tested").clone()
    }

    fn started(&self) -> Vec<(String, IpAddr)> {
        self.started.lock().expect("started").clone()
    }

    fn finished(&self) -> Vec<(String, OidcCallback)> {
        self.finished.lock().expect("finished").clone()
    }

    /// Whether any route reached the manager past its enabled check.
    fn untouched(&self) -> bool {
        self.saved().is_empty()
            && self.tested().is_empty()
            && self.deleted().is_empty()
            && *self.listed.lock().expect("listed") == 0
            && self.started().is_empty()
            && self.finished().is_empty()
    }
}

/// What [`FakeSso`] lists and answers a save with: `provider` with a stored
/// secret.
fn info(provider: OidcProvider) -> OidcProviderInfo {
    OidcProviderInfo {
        callback_url: Some(format!(
            "https://media.example.org/sso/oidc/{}/callback",
            provider.key
        )),
        provider: OidcProvider {
            client_secret: None,
            ..provider
        },
        has_client_secret: true,
        validation_error: None,
        discovery: None,
    }
}

#[async_trait::async_trait]
impl SsoManager for FakeSso {
    async fn ensure_enabled(&self) -> Result<(), ServiceError> {
        if self.enabled {
            Ok(())
        } else {
            Err(ServiceError::not_found(
                "the Single Sign-On plugin is disabled",
            ))
        }
    }

    async fn oidc_providers(&self) -> Result<OidcProviderList, ServiceError> {
        self.ensure_enabled().await?;
        *self.listed.lock().expect("listed") += 1;
        Ok(OidcProviderList {
            public_url: "https://media.example.org".to_owned(),
            providers: vec![info(OidcProvider {
                key: "pocket".to_owned(),
                name: "Pocket ID".to_owned(),
                issuer: "https://id.example.org".to_owned(),
                client_id: "ferrofin".to_owned(),
                ..OidcProvider::default()
            })],
            public_url_error: None,
        })
    }

    async fn save_oidc_provider(
        &self,
        key: &str,
        provider: OidcProvider,
    ) -> Result<OidcProviderInfo, ServiceError> {
        self.ensure_enabled().await?;
        if let Some(why) = &self.refusal {
            return Err(ServiceError::invalid_input(why.clone()));
        }
        self.saved
            .lock()
            .expect("saved")
            .push((key.to_owned(), provider.clone()));
        Ok(info(OidcProvider {
            key: key.to_owned(),
            ..provider
        }))
    }

    async fn delete_oidc_provider(&self, key: &str) -> Result<(), ServiceError> {
        self.ensure_enabled().await?;
        if key != "pocket" {
            return Err(ServiceError::not_found(format!("OIDC provider {key}")));
        }
        self.deleted.lock().expect("deleted").push(key.to_owned());
        Ok(())
    }

    async fn test_oidc_provider(
        &self,
        provider: &OidcProvider,
    ) -> Result<OidcDiscovery, ServiceError> {
        self.ensure_enabled().await?;
        self.tested.lock().expect("tested").push(provider.clone());
        Ok(match &self.discovery_error {
            Some(why) => OidcDiscovery {
                error: Some(why.clone()),
                ..OidcDiscovery::default()
            },
            None => OidcDiscovery {
                authorization_endpoint: Some(format!("{}/authorize", provider.issuer)),
                jwks_uri: Some(format!("{}/jwks", provider.issuer)),
                signing_keys: Some(1),
                ..OidcDiscovery::default()
            },
        })
    }

    async fn start_oidc_sign_in(
        &self,
        key: &str,
        client: IpAddr,
    ) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        if key != "pocket" {
            return Err(ServiceError::not_found(format!("OIDC provider {key}")));
        }
        self.started
            .lock()
            .expect("started")
            .push((key.to_owned(), client));
        Ok(SsoReply {
            status: 302,
            location: Some(AUTHORIZE.to_owned()),
            html: String::new(),
            set_cookies: vec![format!("{OIDC_FLOW_COOKIE}=b1; Path=/sso; HttpOnly")],
        })
    }

    async fn finish_oidc_sign_in(
        &self,
        key: &str,
        callback: OidcCallback,
    ) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        if key != "pocket" {
            return Err(ServiceError::not_found(format!("OIDC provider {key}")));
        }
        self.finished
            .lock()
            .expect("finished")
            .push((key.to_owned(), callback));
        Ok(SsoReply {
            status: 400,
            location: None,
            html: "<p>Sign-in failed. Try again.</p>".to_owned(),
            set_cookies: vec![format!("{OIDC_FLOW_COOKIE}=; Path=/sso; Max-Age=0")],
        })
    }
}

fn as_admin(fake: Arc<FakeSso>) -> AppState {
    authed_state_with_plugins(Arc::new(DisabledPluginManager)).with_sso(fake)
}

fn as_user(fake: Arc<FakeSso>) -> AppState {
    user_authed_state_with_plugins(Arc::new(DisabledPluginManager)).with_sso(fake)
}

/// What the settings page sends for a new provider.
const PAGE_BODY: &str = r#"{"Name":"Pocket ID","Enabled":true,"Issuer":"https://id.example.org",
    "AllowHttpIssuer":false,"ClientId":"ferrofin","Scopes":"openid profile",
    "UsernameClaim":"preferred_username","ClientSecret":"route-client-secret-31"}"#;

/// Each route, with the body the settings page sends.
const PAGE_REQUESTS: [(&str, &str, &str); 3] = [
    ("GET", "/sso/admin/oidc/providers", ""),
    ("PUT", "/sso/admin/oidc/providers/pocket", PAGE_BODY),
    ("DELETE", "/sso/admin/oidc/providers/pocket", ""),
];

fn request(method: &str, path: &str, body: &str, token: bool) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if token {
        builder = builder.header("Authorization", "Token abc");
    }
    if !body.is_empty() {
        builder = builder.header("Content-Type", "application/json");
    }
    builder.body(Body::from(body.to_owned())).expect("request")
}

async fn send(state: AppState, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = create_router(state)
        .oneshot(request)
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, bytes.to_vec())
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("a JSON body")
}

#[tokio::test]
async fn an_admin_lists_the_providers_without_a_secret() {
    let fake = FakeSso::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        request("GET", "/sso/admin/oidc/providers", "", true),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(body["PublicUrl"], "https://media.example.org");
    let provider = &body["Providers"][0];
    assert_eq!(provider["Key"], "pocket");
    assert_eq!(provider["Name"], "Pocket ID");
    assert_eq!(provider["Scopes"], "openid profile");
    assert_eq!(provider["UsernameClaim"], "preferred_username");
    assert_eq!(provider["AllowHttpIssuer"], false);
    assert_eq!(provider["HasClientSecret"], true);
    assert_eq!(
        provider["CallbackUrl"],
        "https://media.example.org/sso/oidc/pocket/callback"
    );
    assert!(provider.get("ClientSecret").is_none(), "{body}");
    assert!(provider.get("ValidationError").is_none(), "{body}");
    assert!(provider.get("Discovery").is_none(), "{body}");
}

#[tokio::test]
async fn an_admin_saves_a_provider_under_the_key_in_its_path() {
    let fake = FakeSso::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        request("PUT", "/sso/admin/oidc/providers/pocket", PAGE_BODY, true),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body_text = String::from_utf8_lossy(&body).into_owned();
    assert!(!body_text.contains(SECRET), "{body_text}");
    let body = json(&body);
    assert_eq!(body["Key"], "pocket");
    assert_eq!(body["HasClientSecret"], true);
    let saved = fake.saved();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].0, "pocket");
    assert_eq!(saved[0].1.key, "", "the body names no key: the path does");
    assert_eq!(saved[0].1.client_id, "ferrofin");
    assert_eq!(
        saved[0]
            .1
            .client_secret
            .as_ref()
            .map(|s| s.expose().to_owned()),
        Some(SECRET.to_owned())
    );
}

#[tokio::test]
async fn a_save_tests_the_saved_provider_and_answers_what_it_found() {
    let fake = FakeSso::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        request("PUT", "/sso/admin/oidc/providers/pocket", PAGE_BODY, true),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(
        body["Discovery"],
        serde_json::json!({
            "AuthorizationEndpoint": "https://id.example.org/authorize",
            "JwksUri": "https://id.example.org/jwks",
            "SigningKeys": 1,
        })
    );
    // The provider as saved is tested, under the key of the path.
    let tested = fake.tested();
    assert_eq!(tested.len(), 1);
    assert_eq!(tested[0].key, "pocket");
    assert_eq!(tested[0].issuer, "https://id.example.org");
    assert_eq!(tested[0].client_secret, None);
}

#[tokio::test]
async fn a_failed_test_still_saves_and_answers_ok_with_the_reason() {
    let fake = Arc::new(FakeSso {
        enabled: true,
        discovery_error: Some("could not connect to the identity provider".to_owned()),
        ..FakeSso::default()
    });
    let (status, body) = send(
        as_admin(fake.clone()),
        request("PUT", "/sso/admin/oidc/providers/pocket", PAGE_BODY, true),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(body["Key"], "pocket");
    assert_eq!(
        body["Discovery"],
        serde_json::json!({"Error": "could not connect to the identity provider"})
    );
    assert_eq!(fake.saved().len(), 1);
}

#[tokio::test]
async fn a_body_without_a_secret_or_settings_binds_to_the_defaults() {
    let fake = FakeSso::new(true);
    // camelCase binds too, as every body does.
    let (status, _) = send(
        as_admin(fake.clone()),
        request(
            "PUT",
            "/sso/admin/oidc/providers/pocket",
            r#"{"name":"Pocket","issuer":"https://id.example.org","clientId":"a"}"#,
            true,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, provider) = fake.saved().remove(0);
    assert_eq!(provider.name, "Pocket");
    assert_eq!(provider.client_id, "a");
    assert_eq!(provider.client_secret, None, "keeps the stored secret");
    assert!(provider.enabled);
    assert_eq!(provider.scopes, "openid profile");
    assert_eq!(provider.username_claim, "preferred_username");
}

#[tokio::test]
async fn a_refused_setting_is_a_bad_request_naming_it() {
    let fake = Arc::new(FakeSso {
        enabled: true,
        refusal: Some("the provider name is required".to_owned()),
        ..FakeSso::default()
    });
    let (status, body) = send(
        as_admin(fake.clone()),
        request("PUT", "/sso/admin/oidc/providers/pocket", PAGE_BODY, true),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json(&body)["error"],
        "invalid input: the provider name is required"
    );
    assert!(fake.tested().is_empty(), "a refused provider is not tested");
}

#[tokio::test]
async fn a_body_that_does_not_bind_is_a_validation_problem() {
    for body in ["not json", "[]", r#"{"Enabled":"yes"}"#] {
        let fake = FakeSso::new(true);
        let (status, answer) = send(
            as_admin(fake.clone()),
            request("PUT", "/sso/admin/oidc/providers/pocket", body, true),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            json(&answer)["title"],
            "One or more validation errors occurred.",
            "{body}"
        );
        assert!(fake.untouched(), "{body}");
    }
}

#[tokio::test]
async fn an_admin_deletes_a_provider_and_an_unknown_one_is_not_found() {
    let fake = FakeSso::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        request("DELETE", "/sso/admin/oidc/providers/pocket", "", true),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Vec::<u8>::new());
    assert_eq!(fake.deleted(), ["pocket"]);
    let (status, _) = send(
        as_admin(fake.clone()),
        request("DELETE", "/sso/admin/oidc/providers/nope", "", true),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_plain_user_is_refused_before_the_plugin_is_reached() {
    for (method, path, body) in PAGE_REQUESTS {
        let fake = FakeSso::new(true);
        let (status, _) = send(as_user(fake.clone()), request(method, path, body, true)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(fake.untouched(), "{method} {path}");
    }
}

#[tokio::test]
async fn an_anonymous_caller_is_unauthorized_while_the_plugin_is_enabled() {
    for (method, path, body) in PAGE_REQUESTS {
        let fake = FakeSso::new(true);
        let (status, _) = send(
            fake_state().with_sso(fake.clone()),
            request(method, path, body, false),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        assert!(fake.untouched(), "{method} {path}");
    }
}

/// B1: a disabled plugin's routes do not exist, for any caller and any body.
#[tokio::test]
async fn a_disabled_plugin_answers_not_found_to_everyone_whatever_the_body() {
    for (method, path, page_body) in PAGE_REQUESTS {
        for body in [page_body, "{}", "not json"] {
            let callers: [(AppState, bool); 3] = [
                (as_admin(FakeSso::new(false)), true),
                (as_user(FakeSso::new(false)), true),
                (fake_state().with_sso(FakeSso::new(false)), false),
            ];
            for (state, token) in callers {
                let (status, _) = send(state, request(method, path, body, token)).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path} {body}");
            }
        }
    }
    // A provider segment that does not decode is not looked at either.
    for method in ["PUT", "DELETE"] {
        let (status, _) = send(
            fake_state().with_sso(FakeSso::new(false)),
            request(method, "/sso/admin/oidc/providers/%FF", "", false),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
    }
}

#[tokio::test]
async fn an_undecodable_provider_segment_is_a_bad_request_for_an_admin() {
    for (method, body) in [("PUT", PAGE_BODY), ("DELETE", "")] {
        let fake = FakeSso::new(true);
        let (status, _) = send(
            as_admin(fake.clone()),
            request(method, "/sso/admin/oidc/providers/%FF", body, true),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method}");
        assert!(fake.untouched(), "{method}");
    }
}

#[tokio::test]
async fn a_server_without_the_extension_answers_not_found() {
    for (method, path, body) in PAGE_REQUESTS {
        let state = authed_state_with_plugins(Arc::new(DisabledPluginManager));
        let (status, _) = send(state, request(method, path, body, true)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }
}

#[tokio::test]
async fn every_sso_plugin_route_is_registered() {
    use ferrofin_api::handlers::{EXTENSION_ROUTES, PLUGIN_ROUTES, REAL_ROUTES};
    let rows: Vec<(&str, &str)> = PLUGIN_ROUTES
        .iter()
        .filter(|(_, _, plugin)| *plugin == "sso")
        .map(|(method, path, _)| (*method, *path))
        .collect();
    assert_eq!(
        rows,
        [
            ("get", "/sso/admin/oidc/providers"),
            ("put", "/sso/admin/oidc/providers/{provider}"),
            ("delete", "/sso/admin/oidc/providers/{provider}"),
            ("get", "/sso/oidc/{provider}/start"),
            ("get", "/sso/oidc/{provider}/callback"),
        ]
    );
    for (method, path) in rows {
        assert!(!REAL_ROUTES.contains(&(method, path)), "{path}");
        assert!(
            !EXTENSION_ROUTES
                .iter()
                .any(|(m, p, _)| *m == method && *p == path),
            "{path}"
        );
        let path = path.replace("{provider}", "pocket");
        let body = if method == "put" { PAGE_BODY } else { "" };
        let (status, _) = send(
            as_admin(FakeSso::new(true)),
            request(&method.to_ascii_uppercase(), &path, body, true),
        )
        .await;
        // The fake's sign-in answers a redirect and a refusal page.
        let expected = match path.as_str() {
            "/sso/oidc/pocket/start" => StatusCode::FOUND,
            "/sso/oidc/pocket/callback" => StatusCode::BAD_REQUEST,
            _ if method == "delete" => StatusCode::NO_CONTENT,
            _ => StatusCode::OK,
        };
        assert_eq!(status, expected, "{method} {path}");
    }
}

/// The sign-in routes: what a browser gets.
async fn respond(state: AppState, request: Request<Body>) -> axum::response::Response {
    create_router(state)
        .oneshot(request)
        .await
        .expect("response")
}

/// A header of `response`, as text.
fn header<'r>(response: &'r axum::response::Response, name: &str) -> Vec<&'r str> {
    response
        .headers()
        .get_all(name)
        .iter()
        .map(|value| value.to_str().expect("text"))
        .collect()
}

#[tokio::test]
async fn start_redirects_an_anonymous_browser_with_the_flow_cookie() {
    let fake = FakeSso::new(true);
    let response = respond(
        fake_state().with_sso(fake.clone()),
        request("GET", "/sso/oidc/pocket/start", "", false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(header(&response, "location"), [AUTHORIZE]);
    assert_eq!(
        header(&response, "set-cookie"),
        ["ferrofin_sso_flow=b1; Path=/sso; HttpOnly"]
    );
    assert_eq!(header(&response, "cache-control"), ["no-store"]);
    // No connection address in a test router: the loopback, as for every
    // other route.
    assert_eq!(
        fake.started(),
        [("pocket".to_owned(), IpAddr::from([127, 0, 0, 1]))]
    );
}

#[tokio::test]
async fn the_callback_hands_its_parameters_and_the_flow_cookie_to_the_plugin() {
    let fake = FakeSso::new(true);
    let callback = Request::builder()
        .uri(
            "/sso/oidc/pocket/callback?state=s1&session_state=x&code=c1\
             &iss=https%3A%2F%2Fid.example.org",
        )
        .header("cookie", "theme=dark; ferrofin_sso_flow=b1")
        .header("cookie", "other=2")
        .body(Body::empty())
        .expect("request");
    let response = respond(fake_state().with_sso(fake.clone()), callback).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        header(&response, "content-type"),
        ["text/html; charset=utf-8"]
    );
    assert_eq!(
        header(&response, "set-cookie"),
        ["ferrofin_sso_flow=; Path=/sso; Max-Age=0"]
    );
    assert_eq!(header(&response, "cache-control"), ["no-store"]);
    assert_eq!(header(&response, "referrer-policy"), ["no-referrer"]);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    assert_eq!(&body[..], b"<p>Sign-in failed. Try again.</p>");

    let (key, callback) = fake.finished().remove(0);
    assert_eq!(key, "pocket");
    assert_eq!(callback.state.as_deref(), Some("s1"));
    assert_eq!(callback.code.as_ref().map(Secret::expose), Some("c1"));
    assert_eq!(callback.iss.as_deref(), Some("https://id.example.org"));
    assert_eq!(callback.error, None);
    assert_eq!(
        callback.flow_cookie.as_ref().map(Secret::expose),
        Some("b1")
    );
}

#[tokio::test]
async fn an_error_answer_reaches_the_plugin_and_a_repeated_parameter_voids_them_all() {
    let fake = FakeSso::new(true);
    let (status, _) = send(
        fake_state().with_sso(fake.clone()),
        request(
            "GET",
            "/sso/oidc/pocket/callback?error=access_denied&state=s1",
            "",
            false,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, callback) = fake.finished().remove(0);
    assert_eq!(callback.error.as_deref(), Some("access_denied"));
    assert_eq!(callback.state.as_deref(), Some("s1"));
    assert!(callback.code.is_none());
    assert!(callback.flow_cookie.is_none());

    for repeated in [
        "state=s2",
        "code=c2",
        "iss=https%3A%2F%2Fevil.example.org",
        "error=x",
    ] {
        let fake = FakeSso::new(true);
        let callback = Request::builder()
            .uri(format!(
                "/sso/oidc/pocket/callback?state=s1&code=c1&iss=https%3A%2F%2Fid.example.org\
                 &error=e1&{repeated}"
            ))
            .header("cookie", "ferrofin_sso_flow=b1")
            .body(Body::empty())
            .expect("request");
        let response = respond(fake_state().with_sso(fake.clone()), callback).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{repeated}");
        let (_, callback) = fake.finished().remove(0);
        assert!(
            callback.state.is_none()
                && callback.code.is_none()
                && callback.iss.is_none()
                && callback.error.is_none(),
            "{repeated}: {callback:?}"
        );
        assert!(callback.flow_cookie.is_some(), "{repeated}");
    }
}

/// B1 for the sign-in routes, and an unknown or undecodable provider: no
/// such route.
#[tokio::test]
async fn sign_in_answers_not_found_while_disabled_or_for_an_unknown_provider() {
    for path in [
        "/sso/oidc/pocket/start",
        "/sso/oidc/pocket/callback?state=s",
    ] {
        let (status, _) = send(
            fake_state().with_sso(FakeSso::new(false)),
            request("GET", path, "", false),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        let (status, _) = send(
            authed_state_with_plugins(Arc::new(DisabledPluginManager)),
            request("GET", path, "", true),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "no extension: {path}");
    }
    for path in [
        "/sso/oidc/nope/start",
        "/sso/oidc/nope/callback",
        "/sso/oidc/%FF/start",
        "/sso/oidc/%FF/callback",
    ] {
        let fake = FakeSso::new(true);
        let (status, _) = send(
            fake_state().with_sso(fake.clone()),
            request("GET", path, "", false),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(fake.untouched(), "{path}");
    }
}
